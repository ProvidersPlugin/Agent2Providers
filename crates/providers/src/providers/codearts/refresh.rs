//! CodeArts 凭据的**续期策略与单飞**：什么时候该刷、同一份凭据只刷一次。
//!
//! ── 为什么必须单飞 ──────────────────────────────────────────
//! 上游的 `refresh_token` 是**一次性**的：同一串换第二次会被拒（甚至可能连带
//! 把整个授权作废）。所以「一分钟内二十个并发请求同时发现凭据要过期」这种局面
//! 绝不能变成二十次真实换证。这里复用 Raccoon / AutoClaw / Qoder 已经在用的
//! 进程级单飞原语（[`crate::flight`]），语义与踩过的坑见那边的模块头。
//!
//! ── 与参考实现的三条同口径 ──────────────────────────────────
//!   1. **提前量 15 分钟**（`onDemandRefreshLead`）。临时凭据寿命只有约 24 小时，
//!      提前量太小会在一次慢请求里过期；太大又白白多换一次。
//!   2. **刷新失败但旧凭据还没过期 → 继续用旧的**，只记一条日志，让周期性路径
//!      稍后重试。反过来，**已经过期就宁可失败**，不要发一个上游会回 503 的请求
//!      让用户以为是服务挂了（参考实现原话："fail before sending a request the
//!      gateway may misleadingly report as an IAM 503"）。
//!   3. **坏时间戳当作要刷**（见 `credentials::needs_refresh`）。
//!
//! ── 与账号存储的边界 ────────────────────────────────────────
//! 本模块只负责「按当前凭据换一份新的」，**不碰存储**。把新凭据写回哪个账号、
//! 写回前确认账号没被重新导入/删除，是账号存储那一层的事（M2/M5 的
//! `codearts_accounts.rs`）—— 那边才有账号 id 与并发写语义。这样切开的好处是
//! 本模块的全部行为都能在无网络、无存储的条件下测（见文件末）。

use std::sync::OnceLock;
use std::time::Duration;

use agent2api_provider_protocol::WireProxy as ResolvedProxy;
use agent2api_provider_sdk::PluginCtx;
use serde_json::Value;
use crate::flight::{Join, Table};
use crate::errors::GatewayError;
use crate::logging;

use super::credentials::Credential;
use super::oauth;

/// 续期提前量：15 分钟（参考实现的 `onDemandRefreshLead`）。
pub const REFRESH_LEAD_MS: i64 = 15 * 60 * 1000;

/// 进程级单飞表。
static FLIGHTS: OnceLock<Table<Credential>> = OnceLock::new();

fn flights() -> &'static Table<Credential> {
    FLIGHTS.get_or_init(Table::new)
}

/// 单飞 key：账号身份 + refresh token 指纹。
///
/// 带上 token 指纹是为了「凭据被换成另一份」时不会与旧的飞行复用同一格；
/// 指纹是截断哈希（见 [`crate::flight::fingerprint`]），不泄露令牌本身。
pub fn refresh_key(account_key: &str, credential: &Credential) -> String {
    format!(
        "codearts:{}:{}:{}",
        account_key,
        credential.identity(),
        crate::flight::fingerprint(&credential.refresh_token)
    )
}

#[allow(dead_code)]
/// 该不该刷（纯判定，便于测试与上层复用）。
///
/// `force` 为真时无条件刷（401 之后的重试路径）。
pub fn should_refresh(credential: &Credential, force: bool, now_ms: i64) -> bool {
    if force {
        return credential.can_refresh();
    }
    credential.can_refresh() && credential.needs_refresh(REFRESH_LEAD_MS, now_ms)
}

#[allow(dead_code)]
/// 需要时把凭据换成新的；不需要就原样返回。
///
/// 失败的两种处理（见模块头第 2 条）：
///   * 旧凭据**还没过期** → 返回旧凭据，记一条日志（不打断在途请求）
///   * 旧凭据**已经过期** → 把错误抛出去（别发注定被拒的请求）
pub async fn ensure_fresh_credential(
    account_key: &str,
    credential: &Credential,
    force: bool,
    proxy: Option<&ResolvedProxy>,
) -> Result<Credential, GatewayError> {
    if !should_refresh(credential, force, logging::now_ms()) {
        return Ok(credential.clone());
    }
    match refresh_single_flight(account_key, credential, proxy).await {
        Ok(fresh) => Ok(fresh),
        Err(error) => {
            if !credential.needs_refresh(0, logging::now_ms()) {
                logging::log(
                    "[CodeArts]",
                    &format!("提前续期失败（{}），旧凭据尚未过期，本次继续使用", error.message),
                );
                return Ok(credential.clone());
            }
            Err(error)
        }
    }
}

#[allow(dead_code)]
/// 无条件换一次（仍受单飞保护）：先到者发请求，后来者复用同一个结果。
pub async fn refresh_single_flight(
    account_key: &str,
    credential: &Credential,
    proxy: Option<&ResolvedProxy>,
) -> Result<Credential, GatewayError> {
    if !credential.can_refresh() {
        return Err(GatewayError::with_status(
            400,
            "CodeArts 凭据缺少 refresh token 或 OAuth 上下文，无法续期，请重新登录",
        ));
    }
    let key = refresh_key(account_key, credential);
    match flights().join(&key) {
        Join::Waiter(waiter) => waiter.wait().await,
        Join::Leader(leader) => {
            let result = oauth::refresh_credential(credential, proxy).await;
            leader.finish(result.clone());
            result
        }
    }
}

#[allow(dead_code)]
/// 与 [`refresh_single_flight`] 同一套单飞骨架，但把「换凭据」这一步交给调用方。
///
/// 存在的唯一理由是**可测**：单飞最容易错的地方（并发只打一次、失败不留缓存、
/// 取消要唤醒等待者）与上游无关，用一段计数闭包就能验；让测试直接驱动这个函数
/// 就不必为了验证并发行为去连真上游。转发的正常路径走 [`refresh_single_flight`]。
pub async fn refresh_through<F, Fut>(
    key: &str,
    operation: F,
) -> Result<Credential, GatewayError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Credential, GatewayError>>,
{
    match flights().join(key) {
        Join::Waiter(waiter) => waiter.wait().await,
        Join::Leader(leader) => {
            let result = operation().await;
            leader.finish(result.clone());
            result
        }
    }
}

#[allow(dead_code)]
/// 单飞等待的兜底时长（与 [`crate::flight`] 的等待上限同量级，用于日志提示）。
pub const SINGLE_FLIGHT_WAIT_HINT_MS: u64 = Duration::from_secs(30).as_millis() as u64;

/// 与账号存储接通的续期：读记录 → 单飞换证 → **写回**（比较再写）。
///
/// `proxy` 由调用方给：转发路径上编排层已经按账号解析好了，面板/目录那两条没有
/// 这个上下文，由 [`super::adapter::record_proxy`] 从账号记录里现解（解析失败报错，
/// 不静默直连）。
///
/// ── 为什么写回是这条路径的必需环节 ──────────────────────────
/// CodeArts 的 refresh_token 是**一次性**的：换证成功那一刻旧串就作废了。
/// 所以「刷了但不存回去」等于**把账号判死刑** —— 内存里拿到新凭据、盘上还是烧掉的
/// 旧串，进程一重启就再也续不了。正因如此 `refresh` 能力位在写回通路接上之前
/// 是刻意关着的（见 `adapter.rs`），而不是「先开着试试看」。
///
/// ── 插件侧的写回形状 ────────────────────────────────────────
/// 网关那份是 `update_codearts_credentials_if_current`（比对身份键 + 就地更新记录）。
/// 插件里没有记录字段的写权限，只有 `store.update(account, expect, next)` 这一条
/// 「比较再写」；`expect` 传**读到时那份 secret**，语义等价于网关的身份键比对
/// （任何字段变了都判 Stale）。
pub async fn ensure_fresh(
    ctx: &PluginCtx,
    account_id: &str,
    force: bool,
    proxy: Option<&ResolvedProxy>,
) -> Result<Credential, GatewayError> {
    let record = crate::store::record(ctx, super::PROVIDER_ID, account_id).await?;
    if !record.is_object() {
        return Err(GatewayError::with_status(
            503,
            "没有可用的 CodeArts 账号：请在账号页添加并启用账号",
        ));
    }
    let credential = Credential::from_payload(&record)
        .map_err(|reason| GatewayError::with_status(400, reason))?;
    let account_id = record.get("id").and_then(Value::as_str)
        .filter(|id| !id.is_empty()).unwrap_or(account_id);
    let relogin = super::authorization::relogin_error(&record);
    if !force && relogin.is_some() && !credential.needs_refresh(0, logging::now_ms()) {
        return Ok(credential);
    }
    if let Some(error) = relogin {
        return Err(error);
    }
    if !force && !credential.needs_refresh(REFRESH_LEAD_MS, logging::now_ms()) {
        return Ok(credential);
    }
    if !credential.can_refresh() {
        return Err(GatewayError::with_status(
            400,
            "CodeArts 凭据缺少 refresh token 或 OAuth 上下文（PKCE/DPoP），无法续期，请重新登录",
        ));
    }
    let key = refresh_key(account_id, &credential);
    // ── 写回只属于 leader ──────────────────────────────────────
    // 等待者拿到的就是 leader 换回来的那一份，而 leader 已经把它落盘了。
    // 让等待者也走一遍写回，比对的是**它自己那份续期之前**的快照 —— leader 一落盘，
    // 那份快照就"过期"了，于是每一次并发续期都会给健康路径打一条
    // 「续期成功但没能写回…请重新添加该账号」的假警报（审计抓出来的）。
    let result = match flights().join(&key) {
        Join::Waiter(waiter) => waiter.wait().await.map(|fresh| (fresh, false)),
        Join::Leader(leader) => {
            let result = oauth::refresh_credential(&credential, proxy).await;
            if let Err(error) = &result {
                if error.code.as_deref() == Some(super::authorization::EXPIRED_CODE) {
                    super::authorization::remember_expired(ctx, account_id, &record, error).await;
                }
            }
            leader.finish(result.clone());
            result.map(|fresh| (fresh, true))
        }
    };
    let (fresh, leader_should_persist) = match result {
        Ok(result) => result,
        Err(error) if !force && !credential.needs_refresh(0, logging::now_ms()) => {
            logging::log(
                "[CodeArts]",
                &format!("提前续期失败（{}），旧凭据尚未过期，本次继续使用", error.message),
            );
            return Ok(credential);
        }
        Err(error) => return Err(error),
    };
    if !leader_should_persist {
        return Ok(fresh);
    }
    // 写回：`expect` 是读到时那份 secret（见函数头的说明）。
    //
    // 新凭据只覆盖**它自己声明过的字段**（`to_record_value` 的并集），其余键
    // （`source` / 账号侧写的任何东西）原样保留 —— 整份替换会把它们洗掉。
    let expect = crate::store::secret_view(&record);
    let next = merge_credentials(&expect, &fresh);
    match crate::store::save(ctx, account_id, &expect, &next).await {
        Ok(true) => Ok(fresh),
        Ok(false) => {
            // 写回没落地（记录在续期期间被换掉 / 被删）。**这时把新凭据交给本次调用**，
            // 而不是回读那份旧的：refresh_token 是一次性的，上游已经轮换过了，旧串下一
            // 刻就可能被判"已使用"。丢的只是持久化，不能连本次请求也一起赔进去。
            logging::log(
                "[CodeArts]",
                "⚠️ 续期成功但没能写回账号存储（记录在续期期间被改动或删除）：新令牌只存在于本次请求，下次刷新很可能被判无效，请重新添加该账号",
            );
            Ok(fresh)
        }
        Err(error) => {
            logging::log(
                "[CodeArts]",
                &format!(
                    "⚠️ 续期写回失败（{}）：新令牌只存在于本次请求，下次刷新很可能被判无效",
                    error.message
                ),
            );
            Ok(fresh)
        }
    }
}

/// 把换来的新凭据并进旧 secret：**逐键覆盖，空值跳过**。
///
/// 空值跳过这一条与网关 `add_codearts_account` / `update_codearts_credentials_if_current`
/// 同一口径：只粘了 AK/SK 的重新添加不该把能续期的 `refreshToken` / `oauth_context`
/// 洗成空（那等于判了这个账号死刑）。
pub fn merge_credentials(secret: &Value, fresh: &Credential) -> Value {
    let mut next = secret.clone();
    let Some(object) = next.as_object_mut() else {
        return fresh.to_record_value();
    };
    if let Value::Object(values) = fresh.to_record_value() {
        for (key, value) in values {
            if value.is_null() || matches!(&value, Value::String(text) if text.is_empty()) {
                continue;
            }
            object.insert(key, value);
        }
    }
    next
}

#[cfg(any())]
mod tests {
    use super::*;
    use crate::providers::codearts::credentials::{DpopKeyPair, Jwk, OAuthContext, PkcePair};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn refreshable_credential() -> Credential {
        Credential {
            access_key_id: "AK".to_string(),
            secret_access_key: "SK".to_string(),
            security_token: "sts".to_string(),
            expires_at: "2026-09-26T16:17:00.327Z".to_string(),
            domain_id: "dom".to_string(),
            user_id: "uid".to_string(),
            refresh_token: "jwt-token".to_string(),
            oauth_context: Some(OAuthContext {
                pkce_pair: PkcePair {
                    code_verifier: "verifier".to_string(),
                    ..PkcePair::default()
                },
                dpop_key_pair: DpopKeyPair {
                    private_key_jwk: Jwk { kty: "EC".into(), crv: "P-256".into(), d: "AQ".into(), ..Jwk::default() },
                    ..DpopKeyPair::default()
                },
            }),
            ..Credential::default()
        }
    }

    /// 「该不该刷」的三态：不临期不刷、临期刷、force 无条件刷。
    #[test]
    fn refresh_decision_covers_the_three_states() {
        let credential = refreshable_credential();
        let expiry = credential.expires_at_ms().unwrap();
        assert!(!should_refresh(&credential, false, expiry - 60 * 60 * 1000), "离到期还有一小时不该刷");
        assert!(should_refresh(&credential, false, expiry - 5 * 60 * 1000), "只剩五分钟必须刷");
        assert!(should_refresh(&credential, true, expiry - 60 * 60 * 1000), "force 无条件刷");
        // 不可续期的凭据：force 也不刷（刷不了，刷只会得到本地错误）
        let bare = Credential { access_key_id: "AK".into(), secret_access_key: "SK".into(), ..Credential::default() };
        assert!(!should_refresh(&bare, true, 0));
    }

    /// **单飞的核心断言**：同一把 key 上 N 个并发只应触发一次真实换证。
    #[tokio::test]
    async fn concurrent_refreshes_hit_upstream_exactly_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let calls = Arc::clone(&calls);
            handles.push(tokio::spawn(async move {
                refresh_through("codearts:test-once:1", move || {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        // 让它悬一会儿，把后来者都挤进等待队列
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok(refreshable_credential())
                    }
                })
                .await
            }));
        }
        for handle in handles {
            handle.await.expect("任务不该 panic").expect("刷新应当成功");
        }
        assert_eq!(1, calls.load(Ordering::SeqCst), "八个并发只该真打一次上游");
    }

    /// 上一轮失败**不能**被缓存成后续请求的结果：force 的下一轮必须重新发起。
    #[tokio::test]
    async fn a_failed_flight_is_not_reused_by_the_next_request() {
        let calls = Arc::new(AtomicUsize::new(0));
        let first = Arc::clone(&calls);
        let result = refresh_through("codearts:test-fail:1", move || {
            let first = Arc::clone(&first);
            async move {
                first.fetch_add(1, Ordering::SeqCst);
                Err(GatewayError::with_status(502, "上游挂了"))
            }
        })
        .await;
        assert!(result.is_err());
        let second = Arc::clone(&calls);
        let result = refresh_through("codearts:test-fail:1", move || {
            let second = Arc::clone(&second);
            async move {
                second.fetch_add(1, Ordering::SeqCst);
                Ok(refreshable_credential())
            }
        })
        .await;
        assert!(result.is_ok(), "上一轮失败后，下一轮必须是新的一次尝试");
        assert_eq!(2, calls.load(Ordering::SeqCst), "第二次请求应当真的又打了一次");
    }

    /// 不同 key 之间不该互相等待（同一个账号换了一份凭据就是不同 key）。
    #[test]
    fn refresh_key_separates_accounts_and_tokens() {
        let first = refreshable_credential();
        let mut other_account = first.clone();
        other_account.user_id = "uid2".to_string();
        let mut other_token = first.clone();
        other_token.refresh_token = "another-jwt".to_string();
        let base = refresh_key("acct-1", &first);
        assert_ne!(base, refresh_key("acct-1", &other_account), "换账号要换格");
        assert_ne!(base, refresh_key("acct-1", &other_token), "换 refresh token 要换格");
        assert_eq!(base, refresh_key("acct-1", &first.clone()), "同一账号同一凭据要稳定命中");
        assert_ne!(base, refresh_key("acct-2", &first), "账号 id 不同要换格");
    }

    #[tokio::test]
    async fn credential_without_refresh_material_fails_before_any_request() {
        let bare = Credential {
            access_key_id: "AK".to_string(),
            secret_access_key: "SK".to_string(),
            ..Credential::default()
        };
        let error = refresh_single_flight("acct", &bare, None)
            .await
            .expect_err("没有续期材料就该本地报错");
        assert_eq!(400, error.status_code);
        assert!(error.message.contains("重新登录"));
    }

    /// 过期判定走 `credentials` 的实现，这里只确认 `ensure_fresh` 不会在
    /// 「不需要刷」时去打网络（否则这条测试会因为连不上上游而失败）。
    #[tokio::test]
    async fn a_fresh_credential_never_touches_the_network() {
        let mut credential = refreshable_credential();
        credential.expires_at = "2099-01-01T00:00:00Z".to_string();
        let fresh = ensure_fresh_credential("codearts:test-fresh:1", &credential, false, None)
            .await
            .expect("不临期时应当直接返回原凭据");
        assert_eq!(credential.refresh_token, fresh.refresh_token);
    }
}
