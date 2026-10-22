//! 单飞刷新：同一凭证的并发刷新只打一次上游。
//!
//! ── 这个模块解决什么（网关侧同名原语的精简版）────────────────
//! 两家同时发现 token 临期时会各自去打刷新接口，而多数上游的刷新是
//! **一次性**的（旧 refresh_token 换新之后立即失效）—— 后到的那次必然
//! 失败，并且可能把刚拿到的新凭证覆盖成坏的。
//!
//! 插件里只有**一个进程**，所以不需要网关那套跨账号的 watch 表：一把
//! 按 key 分的互斥就够。key 由调用方构造（各家凭证的身份字段不同）。
//!
//! ── 与网关实现的差别（有意为之）─────────────────────────────
//! 网关那版要处理「future 被 drop 时释放占位」（转发链路的 future 随时可能
//! 被客户端断开而丢弃）。插件侧的刷新发生在 `token.ensure` / `token.refresh`
//! 这类短调用里，不会被长连接拖住；而 `Mutex` 的守卫在 future 被 drop 时
//! 也会释放，所以不需要额外的 RAII 表项清理。
//!
//! ── 用法 ────────────────────────────────────────────────────
//! ```ignore
//! let next = flight::run(&key, || async {
//!     // 这里做真正的刷新；返回值会交给所有等待者
//!     refresh_once().await
//! }).await?;
//! ```
//! **`run` 内部的闭包只会被执行一次**（后到者等前一个完成后直接拿它的
//! 结果，不重新执行）。这意味着调用方应当在拿到结果后**重新读一次账号**
//! —— 前一个可能已经写入了新凭证。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::Mutex as AsyncMutex;

/// 按 key 分的互斥表。
fn table() -> &'static Mutex<HashMap<String, Arc<AsyncMutex<()>>>> {
    static TABLE: OnceLock<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取一个 key 对应的互斥（不存在则建）。
fn lock_for(key: &str) -> Arc<AsyncMutex<()>> {
    if let Ok(mut map) = table().lock() {
        return map.entry(key.to_string()).or_insert_with(|| Arc::new(AsyncMutex::new(()))).clone();
    }
    // 表被毒化了：退化成一个不共享的锁（并发保护失效但链路不断）。
    // 这与「刷新可能重复一次」的代价相比是更安全的方向 —— 后者会让
    // 整条请求失败。
    Arc::new(AsyncMutex::new(()))
}

/// 单飞执行：同 key 的并发调用串行，且**只有第一个真正执行闭包**。
///
/// `key` 应当带上账号 id 与凭证来源（例如 `format!("{provider}:{account}")`）。
/// 不要把凭证本体放进 key —— 它在日志里可能被打印。
pub async fn run<T, F, Fut>(key: &str, work: F) -> Result<T, String>
where
    T: Clone,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let lock = lock_for(key);
    // 拿到锁说明前一个（如果有）已经完成 —— 它的结果已经写回账号记录，
    // 调用方应当在拿到锁之后**重新读账号**再决定要不要真的刷新。
    let _guard = lock.lock().await;
    work().await
}

/// 带「已完成则跳过」的单飞：`check` 返回 `Some(value)` 时不再执行闭包。
///
/// 目前没有调用方（各家自己用 [`run`] + 锁内重读账号达成同一效果），
/// 保留是因为「串行化」与「复查」这两件事分开表达更清楚。
#[allow(dead_code)]
///
/// ── 什么时候用这个而不是 [`run`] ────────────────────────────
/// 刷新路径的常见形态是「先看还需不需要刷，需要才刷」。只串行不复查的话，
/// 十个并发请求会**依次**各刷一次（每次都用最新的 refresh_token，所以不会
/// 失败，但白白打十次上游）。这里在拿到锁之后、执行之前再问一次 `check`。
pub async fn run_if<T, C, F, Fut>(key: &str, check: C, work: F) -> Result<T, String>
where
    T: Clone,
    C: FnOnce() -> Option<T>,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let lock = lock_for(key);
    let _guard = lock.lock().await;
    if let Some(done) = check() {
        return Ok(done);
    }
    work().await
}

// ─── `Table`：与网关 `core::providers::refresh_flight` 同形的单飞表 ───
//
// 上面两个函数是「一把钥匙一把锁」的简化版；下面这套是从网关逐字搬来的，
// 因为搬过来的各家 `credentials.rs` 用的是它的 `join` / `Join` / `finish` 三件套。
// 两套共存的代价只是几十行，收益是那几家的刷新编排**一行都不用改**。

/// 凭证指纹：截断的 SHA-256 十六进制（32 字符）。
///
/// 只用于单飞表的 key —— 既让「同一凭证」稳定命中同一格，又不在 key 里长期
/// 持有完整 token。它与 token 同等敏感：**绝不写进日志或错误文案**。
pub fn fingerprint(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        // 写进 String 不会失败（fmt::Write for String 恒为 Ok）
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// 等一次并发刷新的上限（150 × 200ms = 30 秒）。
const WAIT_STEPS: usize = 150;

/// 每次等待的间隔（毫秒）
const WAIT_STEP_MS: u64 = 200;

/// leader 被取消（future drop）时等待者看到的错误文案
pub const CANCELLED_MESSAGE: &str = "刷新已被取消，请稍后重试";

/// 等待超时的错误文案
pub const WAIT_TIMEOUT_MESSAGE: &str = "token 正在刷新中，请稍后重试";

/// 一次进行中的刷新。
struct Flight<T> {
    /// 结果槽：`None` = 仍在刷新；`Some` = 已落地（成功或失败）
    done: std::sync::Mutex<Option<Result<T, crate::errors::GatewayError>>>,
    /// 完成信号的版本号（自增；只用于唤醒 `watch` 订阅者）
    version: std::sync::atomic::AtomicU64,
    /// 完成信号：`send` 唤醒所有订阅者
    signal: tokio::sync::watch::Sender<u64>,
}

impl<T> Flight<T> {
    fn new() -> Self {
        let (signal, _receiver) = tokio::sync::watch::channel(0u64);
        Self {
            done: std::sync::Mutex::new(None),
            version: std::sync::atomic::AtomicU64::new(0),
            signal,
        }
    }

    /// 取结果槽（锁在返回前释放）
    fn outcome(&self) -> Option<Result<T, crate::errors::GatewayError>>
    where
        T: Clone,
    {
        let guard = match self.done.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.clone()
    }

    /// 写结果（先到者生效；一个 flight 只会有一次写入）
    fn set_outcome(&self, result: Result<T, crate::errors::GatewayError>) {
        let mut guard = match self.done.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.is_none() {
            *guard = Some(result);
        }
    }

    /// 发完成信号（结果已经写好之后调用）
    fn signal(&self) {
        let next = self.version.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        // 没有订阅者时 `send` 返回 Err —— 不是错误（leader 自己不需要通知）
        let _ = self.signal.send(next);
    }
}

/// 单飞表：key → 正在进行的刷新。
///
/// 表里**只有进行中的 flight**：结束（成功/失败/取消）即移除，因此表的大小受
/// 「同时在飞的刷新数」限制，不会随请求量增长，也不会跨轮次留下旧结果。
pub struct Table<T> {
    entries: std::sync::Mutex<
        std::collections::HashMap<String, std::sync::Arc<Flight<T>>>,
    >,
}

impl<T> Default for Table<T> {
    fn default() -> Self {
        Self {
            entries: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

impl<T> Table<T> {
    /// 建一张空表（调用方通常放进 `OnceLock` 做进程级单例）
    pub fn new() -> Self {
        Self::default()
    }

    /// 取表锁（锁中毒不致命：与账号存储同一策略）
    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, std::sync::Arc<Flight<T>>>>
    {
        match self.entries.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// 移除表项（只移除仍是自己的那一格，绝不误删后来者的 flight）
    fn detach(&self, key: &str, flight: &std::sync::Arc<Flight<T>>) {
        let mut entries = self.lock();
        if let Some(current) = entries.get(key) {
            if std::sync::Arc::ptr_eq(current, flight) {
                entries.remove(key);
            }
        }
    }
}

impl<T: Clone + Send + 'static> Table<T> {
    /// 加入（或发起）一次单飞刷新。
    ///
    /// 返回 [`Join::Leader`] 时本调用抢到了刷新权：做完真实请求后**必须**调用
    /// `guard.finish(result)`（成功与失败都要）；直接 drop 等价于「取消」，
    /// 等待者会收到 [`CANCELLED_MESSAGE`]。
    pub fn join(&self, key: &str) -> Join<'_, T> {
        let mut entries = self.lock();
        if let Some(flight) = entries.get(key) {
            // 订阅先于返回：命中这一格蕴含「信号尚未发出」（发出前表项已移除），
            // 所以这次订阅不会错过唤醒
            let receiver = flight.signal.subscribe();
            let flight = std::sync::Arc::clone(flight);
            drop(entries);
            return Join::Waiter(Waiter { flight, receiver });
        }
        let flight = std::sync::Arc::new(Flight::new());
        entries.insert(key.to_string(), std::sync::Arc::clone(&flight));
        drop(entries);
        Join::Leader(LeaderGuard {
            key: key.to_string(),
            flight,
            table: self,
            finished: false,
        })
    }
}

/// `Table::join` 的结果
pub enum Join<'a, T> {
    /// 本调用是 leader（持有 RAII 守卫）
    Leader(LeaderGuard<'a, T>),
    /// 本调用是等待者
    Waiter(Waiter<T>),
}

/// leader 的 RAII 守卫：正常结束走 [`LeaderGuard::finish`]，被取消走 `Drop`。
pub struct LeaderGuard<'a, T> {
    key: String,
    flight: std::sync::Arc<Flight<T>>,
    table: &'a Table<T>,
    finished: bool,
}

impl<'a, T> LeaderGuard<'a, T> {
    /// 落地本轮结果（成功与失败都走这里）。
    ///
    /// 顺序是**先移除表项、再写结果**：这样 `force = true` 的下一轮一定发起
    /// 新请求，而不是撞上刚完成的这一格去复用旧结果。
    pub fn finish(mut self, result: Result<T, crate::errors::GatewayError>) {
        self.table.detach(&self.key, &self.flight);
        self.finished = true;
        self.flight.set_outcome(result);
        self.flight.signal();
    }
}

impl<'a, T> Drop for LeaderGuard<'a, T> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // 取消路径（future 被 drop / 提前返回）：释放占位并唤醒等待者，
        // 让后续请求可以重新发起刷新
        self.table.detach(&self.key, &self.flight);
        self.flight.set_outcome(Err(crate::errors::GatewayError::with_status(
            409,
            CANCELLED_MESSAGE,
        )));
        self.flight.signal();
    }
}

/// 等待者句柄
pub struct Waiter<T> {
    flight: std::sync::Arc<Flight<T>>,
    receiver: tokio::sync::watch::Receiver<u64>,
}

impl<T: Clone> Waiter<T> {
    /// 等那一轮的结果（leader 成功/失败/取消都会结束等待）。
    pub async fn wait(mut self) -> Result<T, crate::errors::GatewayError> {
        for _ in 0..WAIT_STEPS {
            if let Some(outcome) = self.flight.outcome() {
                return outcome;
            }
            let waited = tokio::time::timeout(
                std::time::Duration::from_millis(WAIT_STEP_MS),
                self.receiver.changed(),
            )
            .await;
            match waited {
                // 信号到达：下一轮读结果
                Ok(Ok(())) => {}
                // 发送端关闭：不会发生（leader 结束前一直持有 Arc），按超时继续轮询
                Ok(Err(_)) => {}
                // 本步超时：继续轮询（覆盖「leader 卡在网络上」的场景）
                Err(_) => {}
            }
        }
        Err(crate::errors::GatewayError::with_status(
            409,
            WAIT_TIMEOUT_MESSAGE,
        ))
    }
}
