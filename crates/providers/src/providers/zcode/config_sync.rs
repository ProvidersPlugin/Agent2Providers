//! 把上游的两份「会变的配置」**同步进来**的后台循环：
//!   1. 风控配置（`client/configs` 的 `captcha` 段）→ [`super::captcha`]；
//!   2. 端点映射（`agent/configs` 的 `proxyEndpoint.mapping`）→ [`super::endpoint_mapping`]。
//!
//! ── 为什么需要一个循环 ─────────────────────────────────────
//! 活动套餐通道按「每条请求都要一枚阿里云验证码令牌」实现（2026-09-28 实测
//! 缺头必回 3007）。上游自己下发了一份风控配置：
//!
//! ```text
//!   GET https://zcode.z.ai/api/v1/client/configs?app_version=…&platform=…
//!     data.configs.captcha = {"enabled":true,"prefix":"no8xfe","region":"cn",
//!                             "sceneId":"11xygtvd","skip_model_request":true}
//! ```
//!
//! 其中的 `enabled` 是**会变的**：上游今天关掉验证、明天打开，都不需要提前发版。
//! 所以它必须被周期性读回来，而不是编译期写死。
//!
//! `enabled` 与 `skip_model_request` 共同决定推理是否需要令牌；配置缺失不放行，
//! 实际收到 3007 后恢复验证要求，不被后续缓存配置覆盖。
//!
//! 端点映射（第 2 份）是同一类东西：上游通过它把推理端点在平面间迁移
//! （`open.bigmodel.cn` → `zcode.z.ai`，见 `endpoint_mapping` 的模块头），表随
//! 服务端变。两条配置的性质相同 —— 低频、与账号无关、会反转 —— 就共用这一条
//! 5 分钟的循环（各自挑各自的代表账号、各自失败互不影响）。
//!
//! 首个套餐池请求等待风控配置同步，避免默认要码状态提前拦下合法免码请求。
//! 后续请求复用缓存；端点映射仍在后台同步，不阻塞推理。
//!
//! ── 三条口径 ────────────────────────────────────────────────
//!   1. **没有账号走活动套餐就一次都不问**：这份配置只对那条通道有意义，问了也
//!      没人读（与令牌池守卫「没有账号用这条路时一个都不铸」同一取向）；
//!   2. **问不到就保持现值**：网络失败 / 上游没给 captcha 段，都不构成「免码」的
//!      证据（判错的代价是逐条 3007，见 `captcha::required` 那一段的取舍）；
//!   3. **翻转会打日志**，且免码时顺手撤掉本地闸门（[`captcha::set_required`] 里做）
//!      —— 闸门存在的前提正是「这条通道要令牌而本机铸不出」，前提没了就不能还挡着。
//!
//! ── 硬约束 ────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::Value;

use agent2api_provider_protocol::WireProxy as ResolvedProxy;
use agent2api_provider_sdk::PluginCtx;

use crate::logging;

use super::{captcha, claim, endpoint_mapping, plan_of, region::Region, PLAN_START};

/// 同步间隔（毫秒）。
///
/// 5 分钟：这份配置上游自己说的是「客户端启动时取一次」（参考实现带 60 秒缓存），
/// 而我们拿它当一个可能反转的开关用 —— 太密是纯噪声，太疏则上游改回去以后要撞
/// 很久。5 分钟落在「一次误判最多损失几分钟流量」这一侧。
pub const SYNC_INTERVAL_MS: u64 = 5 * 60_000;

/// 启动后第一次同步的延迟。
///
/// 3 秒：等账号存储把盘上的记录读稳。这一枪决定「网关重启后多久恢复活动套餐
/// 通道」—— 没有它的话，重启后的第一条请求会先撞一次本地闸门（120 秒）才被放行。
#[allow(dead_code)]
// 启动延迟在插件里没有消费方（同步不再常驻，见 `maybe_sync`），保留是为了
// 与网关那份逐条对照：它解释「网关重启后多久恢复活动套餐通道」。
const FIRST_DELAY_MS: u64 = 3_000;

/// 首次套餐池请求先取得风控配置，避免后台同步尚未完成时误判令牌池为空。
pub async fn ensure_captcha_config(region: Region, proxy: Option<&ResolvedProxy>) {
    use std::sync::OnceLock;

    static SYNC: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let fresh = || captcha::last_sync_at() > 0
        && logging::now_ms().saturating_sub(captcha::last_sync_at()) < SYNC_INTERVAL_MS as i64;
    if fresh() { return; }
    let _guard = SYNC.get_or_init(|| tokio::sync::Mutex::new(())).lock().await;
    if fresh() { return; }
    match claim::captcha_config(region, proxy).await {
        Ok(Some(config)) => captcha::apply_config(&config),
        Ok(None) => {
            captcha::note_synced();
            captcha::set_required(true);
        }
        Err(error) => {
            captcha::set_required(true);
            logging::verbose(
                "[ZCode]",
                &format!("获取套餐池的风控配置失败（保持验证要求）: {}", error.message),
            );
        }
    }
}

pub async fn maybe_sync(ctx: &PluginCtx) {
    if !due() {
        return;
    }
    let mut jobs: Vec<SyncJob> = Vec::new();
    if let Some((region, account_id)) = start_plan_account(ctx).await {
        let proxy = account_proxy(ctx, region, &account_id).await;
        jobs.push(SyncJob::Captcha { region, proxy });
    }
    for region in Region::ALL {
        if endpoint_mapping::disabled(region) {
            endpoint_mapping::clear(region);
            continue;
        }
        if let Some((account_id, credential)) = mapping_account(ctx, region).await {
            let proxy = account_proxy(ctx, region, &account_id).await;
            jobs.push(SyncJob::Mapping {
                region,
                credential,
                proxy,
            });
        }
    }
    if jobs.is_empty() {
        return;
    }
    tokio::spawn(async move {
        for job in jobs {
            run_job(job).await;
        }
    });
}

/// 一件待办（只装可以跨线程搬走的数据，不装 ctx）。
enum SyncJob {
    Captcha {
        region: Region,
        proxy: Option<ResolvedProxy>,
    },
    Mapping {
        region: Region,
        credential: String,
        proxy: Option<ResolvedProxy>,
    },
}

/// 距上次同步是否已够一个周期（并且没有一轮在跑）。
fn due() -> bool {
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
    static LAST: AtomicI64 = AtomicI64::new(0);
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if RUNNING.swap(true, Ordering::SeqCst) {
        return false;
    }
    let now = logging::now_ms();
    let last = LAST.load(Ordering::SeqCst);
    let ready = last == 0 || now.saturating_sub(last) >= SYNC_INTERVAL_MS as i64;
    if ready {
        LAST.store(now, Ordering::SeqCst);
    }
    RUNNING.store(false, Ordering::SeqCst);
    ready
}

/// 在后台任务里执行一件待办（与网关 `sync_captcha_config` /
/// `sync_endpoint_mapping` 的单轮逻辑逐条相同）。
async fn run_job(job: SyncJob) {
    match job {
        SyncJob::Captcha { region, proxy } => {
            match claim::captcha_config(region, proxy.as_ref()).await {
                Ok(Some(config)) => captcha::apply_config(&config),
                Ok(None) => {
                    captcha::note_synced();
                    captcha::set_required(true);
                }
                // 失败**不动结论**：网络/代理/temporarily 5xx 都不构成免码的证据
                Err(error) => logging::verbose(
                    "[ZCode]",
                    &format!("同步套餐池的风控配置失败（保持现值）: {}", error.message),
                ),
            }
        }
        SyncJob::Mapping {
            region,
            credential,
            proxy,
        } => {
            match endpoint_mapping::refresh(region, Some(&credential), proxy.as_ref()).await {
                Ok((count, changed)) => {
                    if changed {
                        logging::log(
                            "[ZCode]",
                            &format!(
                                "{} 的推理端点映射已更新（{count} 条，来源 GET /api/v1/agent/configs）",
                                region.label()
                            ),
                        );
                    }
                }
                Err(error) => logging::verbose(
                    "[ZCode]",
                    &format!(
                        "同步 {} 的端点映射失败（保持现值）: {}",
                        region.label(),
                        error.message
                    ),
                ),
            }
        }
    }
}

/// 端点映射的代表账号：该地区**启用**的 ZCode 账号里第一个带编码套餐 API Key 的
/// （返回 `(账号 id, API Key)`）。
///
/// 通道不限 —— 映射对两条通道的推理 URL 都可能生效；判「启用」的口径与
/// [`pick_start_plan_account`] 相同（`enabled == Some(false)` 才算禁用）。
async fn mapping_account(ctx: &PluginCtx, region: Region) -> Option<(String, String)> {
    let accounts = list_accounts(ctx, region).await;
    for account in &accounts {
        if account.get("enabled").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        let id = account
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if id.is_empty() {
            continue;
        }
        let key = account
            .get("accessToken")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !key.is_empty() {
            return Some((id.to_string(), key.to_string()));
        }
    }
    None
}

/// 有没有「启用 + 走活动套餐」的账号，有的话取第一个（连同它的地区与 id）。
async fn start_plan_account(ctx: &PluginCtx) -> Option<(Region, String)> {
    for region in Region::ALL {
        let accounts = list_accounts(ctx, region).await;
        if let Some(found) = pick_start_plan_account(&accounts) {
            return Some(found);
        }
    }
    None
}

/// 一个地区的账号快照（插件侧形态：每条已带 `id` / `provider` 与摊平的 secret）。
async fn list_accounts(ctx: &PluginCtx, region: Region) -> Vec<Value> {
    crate::store::list(ctx, region.provider_id())
        .await
        .unwrap_or_default()
}

/// 账号级出口（网关 `proxies::session_proxy` 的同义物）。
async fn account_proxy(
    ctx: &PluginCtx,
    region: Region,
    account_id: &str,
) -> Option<ResolvedProxy> {
    let record = crate::store::record(ctx, region.provider_id(), account_id)
        .await
        .ok()?;
    record
        .get("proxy")
        .and_then(|value| serde_json::from_value::<ResolvedProxy>(value.clone()).ok())
}

/// 筛选本体（纯函数：喂一份账号快照就能验，见测试模块）。
///
/// 三条判据缺一不可：
///   · provider id 认得出是 ZCode 系（国内 / 国际两个 id 都算）；
///   · **禁用账号不算** —— 那条通道此刻没有流量，问了也没人读；
///     判据是 `enabled == Some(false)`，缺键按启用处理（与选路侧同一口径）；
///   · 通道取值是活动套餐（编码套餐那条压根不要令牌，问了也没有读者）。
fn pick_start_plan_account(items: &[Value]) -> Option<(Region, String)> {
    for account in items {
        let provider = account
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("");
        let Some(region) = Region::from_provider_id(provider) else {
            continue;
        };
        if account.get("enabled").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        if plan_of(account) != PLAN_START {
            continue;
        }
        let id = account
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if id.is_empty() {
            continue;
        }
        return Some((region, id.to_string()));
    }
    None
}

#[cfg(test)]
mod sync_rules {
    //! 「该不该问、问到了怎么落结论」这一侧的判据。
    //!
    //! 网络那一半（`claim::captcha_config`）不在这里测：它的响应解析有专门的
    //! 用例钉在抓下来的真实载荷上（见 `claim` 的测试模块）。这里只钉循环改得
    //! 最多的两件事：候选账号的筛选，和 `required` 结论的落点。

    use super::*;
    use serde_json::json;

    #[test]
    fn only_a_live_account_on_the_activity_channel_makes_us_ask() {
        let intl = json!({"id": "zcode-intl-user-2", "provider": "zcode-intl",
                          "enabled": true, "zcodePlan": PLAN_START});
        let coding = json!({"id": "zcode-user-1", "provider": "zcode",
                            "enabled": true, "zcodePlan": "coding-plan"});
        let disabled = json!({"id": "zcode-user-9", "provider": "zcode",
                              "enabled": false, "zcodePlan": PLAN_START});
        let other_home = json!({"id": "trae-1", "provider": "trae", "enabled": true});
        let no_id = json!({"provider": "zcode", "enabled": true, "zcodePlan": PLAN_START});

        // 国际版也在这一条链上（它是另一个 provider id，不是同一个家的别名）
        assert_eq!(
            Some((Region::Intl, "zcode-intl-user-2".to_string())),
            pick_start_plan_account(&[coding.clone(), intl.clone()]),
            "走活动套餐的那个账号被挑出来，编码套餐的不算"
        );
        assert_eq!(
            None,
            pick_start_plan_account(&[coding.clone(), disabled.clone(), other_home, no_id]),
            "禁用 / 别家 / 缺 id 的都不构成「该问一次」的理由 —— 全都挡掉后一轮 GET 都不发"
        );
        // 缺 `enabled` 键按启用处理（与选路侧同一口径：只有 `Some(false)` 才是禁用）
        assert_eq!(
            Some((Region::Cn, "zcode-user-7".to_string())),
            pick_start_plan_account(&[json!({"id": "zcode-user-7", "provider": "zcode",
                                             "zcodePlan": PLAN_START})]),
            "老记录里没有 enabled 键，不该把这一家当成没人用"
        );
        // 多个候选时取**第一个**：每轮只问一个账号，别跳着换着问
        // （那份配置与账号无关，换着问只是多打几次同一个 GET）
        assert_eq!(
            Some((Region::Cn, "zcode-user-3".to_string())),
            pick_start_plan_account(&[
                json!({"id": "zcode-user-3", "provider": "zcode",
                       "enabled": true, "zcodePlan": PLAN_START}),
                json!({"id": "zcode-user-4", "provider": "zcode-intl",
                       "enabled": true, "zcodePlan": PLAN_START}),
            ]),
        );
        assert_eq!(
            None,
            pick_start_plan_account(&[disabled, coding]),
            "全是非候选时一个都不该被挑（此时循环一轮 GET 都不发）"
        );
    }

    #[test]
    fn skipping_the_token_header_is_the_inverse_of_requiring_it() {
        // 结论的写入口只有 `captcha::set_required` 一个（`apply_config` 与 3007
        // 两处都经它）。这里钉的是两侧都有明确落点：翻到免码 ⇒ `plan.rs` 不再
        // 取令牌；翻回要码 ⇒ 又取。
        // 环境变量按下时本用例的前提不成立，直接如实失败（而不是静默通过）。
        let _slot = crate::readiness::lock_for_tests();
        assert!(
            captcha::required(),
            "默认结论必须是「要码」：headless 上判错的代价不对称"
        );
        assert!(
            captcha::set_required(false),
            "从要码翻到免码应当报出「改变了」"
        );
        assert!(!captcha::required(), "免码落地后不该再去取令牌");
        assert!(
            !captcha::set_required(false),
            "同一个结论重复写不算改变（逐轮同步不会刷屏）"
        );
        assert!(captcha::set_required(true), "翻回去同样是一次改变");
        assert!(captcha::required());
    }
}
