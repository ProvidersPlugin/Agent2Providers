//! ZCode 的 CLI 轮询登录，改造成插件协议的「轮询型」。
//!
//! ── 链路（三步）──────────────────────────────────────────
//! ```text
//!   1. POST {zcode}/api/v1/oauth/cli/init      → flow_id + authorize_url
//!   2. 用户在浏览器里授权（地址已补好中转页参数）
//!   3. GET  {zcode}/api/v1/oauth/cli/poll/{id} → 访问令牌 + 套餐 JWT
//! ```
//!
//! 第 3 步拿到的 `access_token` **不能直接用于推理**：要先拿它换一把编码套餐
//! 的 API Key（见 [`super::coding_key`]）。这一步必须做，否则账号落下来是
//! 「能领套餐但发不出请求」—— 与参考实现的结论一致。
//!
//! ── 与网关那份的差别（迁移时改的）───────────────────────────
//! 网关的 `start_zcode_login` 是**一个阻塞到结束的后台任务**；插件协议里登录
//! 是「开始 → 反复轮询」两段，所以拆成：
//!
//! | 网关 | 插件 |
//! |---|---|
//! | `start_zcode_login` 起 spawn 任务 | [`login_start`]：调 `CliLogin::start`，回授权地址 |
//! | 任务内 while 循环 | [`login_poll`]：每次只轮询**一次**，由网关按间隔调 |
//! | `store.add_zcode_account` | 返回 `NormalizedAccount`，由网关落库 |
//!
//! ── 流程状态放哪 ────────────────────────────────────────────
//! `CliLogin` 持有 `flow_id` / `poll_token` / 地区 / 设备标识，跨调用要留着。
//! 插件是**单进程长驻**的，所以放进程内的表里（键是 `flow_id`，与网关的
//! 「按 flow 找任务」同一口径）。超时清理由 [`login_cancel`] 与「同一 flow
//! 已被清理」两条路径覆盖 —— 网关侧的登录任务有自己的 5 分钟总超时。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use agent2api_provider_protocol as proto;

use crate::errors::GatewayError;

use super::oauth::CliLogin;
use super::region::Region;
use super::{account_view, identity_of};

/// 流程表：`flow_id` → 已发起的那次登录。
///
/// 存 `Arc` 而不是值：`poll` 是 async，而 `std::sync::MutexGuard` 不能跨
/// await 持有（那会让 future 不是 `Send`）。把条目 clone 出表、放锁之后再
/// 发网络请求，锁只在一次 `get` 里持有。
fn flows() -> &'static Mutex<HashMap<String, std::sync::Arc<CliLogin>>> {
    static FLOWS: OnceLock<Mutex<HashMap<String, std::sync::Arc<CliLogin>>>> = OnceLock::new();
    FLOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 发起登录。
///
/// `method` 目前只有一个取值（`"web"`，界面上叫「网页登录」）；其余取值
/// 如实报错而不是静默按网页登录处理 —— 静默会让「点了另一个按钮却跑了
/// 这套流程」变成查不出来的事。
pub(super) async fn login_start(
    region: Region,
    method: &str,
) -> Result<proto::LoginStart, GatewayError> {
    if !matches!(method, "" | "web" | "browser") {
        return Err(GatewayError::with_status(
            400,
            format!("ZCode 不支持登录方式「{method}」（可用：web）"),
        ));
    }
    let flow = CliLogin::start(region).await?;
    let state = flow.state().to_string();
    let auth_url = flow.auth_url().to_string();
    // 轮询间隔由上游给（`poll_interval_sec`，下限 1 秒）；协议要毫秒。
    let poll_interval_ms = flow.poll_interval().as_millis() as u64;
    if let Ok(mut table) = flows().lock() {
        // 顺手清掉过期条目：网关的任务表按 5 分钟总超时收尾，这里按同一个
        // 量级留 10 分钟 —— 比它更老的 flow 不可能还有人问。
        let now = crate::logging::now_ms();
        table.retain(|_, entry| now.saturating_sub(entry_started_at(entry)) < FLOW_KEEP_MS);
        table.insert(state.clone(), std::sync::Arc::new(flow));
    } else {
        return Err(GatewayError::with_status(
            500,
            "登录流程表不可用（锁中毒），请重启插件后重试",
        ));
    }
    Ok(proto::LoginStart {
        flow: state,
        url: auth_url,
        instructions: "在打开的页面里完成授权，完成后本页会自动继续".to_string(),
        poll_interval_ms,
        // 授权页在**上游域名**终结、回调永远不回到本机 —— 由网关轮询收尾
        external_browser: true,
        allowed_hosts: vec!["zcode.z.ai".to_string()],
    })
}

/// 轮询一次。
pub(super) async fn login_poll(region: Region, flow_id: &str) -> Result<proto::LoginPoll, GatewayError> {
    // 把条目 clone 出表（`Arc` 克隆，不复制令牌），**放锁之后**再 await ——
    // `MutexGuard` 跨 await 会让 future 不是 `Send`。
    let flow = {
        let Ok(table) = flows().lock() else {
            return Ok(proto::LoginPoll::Failed {
                message: "登录流程表不可用（锁中毒），请重新发起登录".to_string(),
            });
        };
        match table.get(flow_id) {
            Some(flow) if flow.region() == region => Some(std::sync::Arc::clone(flow)),
            Some(_) => {
                return Ok(proto::LoginPoll::Failed {
                    message: "这条登录流程属于另一个地区，请重新发起".to_string(),
                })
            }
            None => None,
        }
    };
    let Some(flow) = flow else {
        return Ok(proto::LoginPoll::Failed {
            message: "登录流程不存在或已过期，请重新发起".to_string(),
        });
    };
    match flow.poll().await {
        Ok(None) => Ok(proto::LoginPoll::Pending { hint: None }),
        Ok(Some(mut credentials)) => {
            // ── 先把 OAuth 令牌换成推理凭证 ────────────────────────
            // 轮询给的 `access_token` 直接用于推理必 401，要先换一把编码套餐
            // 的 API Key（见模块头）。换取失败即登录失败：不落半条「能领套餐
            // 但发不出请求」的账号。
            match super::coding_key::resolve(credentials.region, &credentials.access_token).await {
                Ok(key) => credentials.access_token = key,
                Err(error) => {
                    // 流程已用尽：从表里摘掉，让下一次轮询得到「已过期」
                    // 而不是反复拿同一个坏令牌去换
                    if let Ok(mut table) = flows().lock() {
                        table.remove(flow_id);
                    }
                    return Ok(proto::LoginPoll::Failed {
                        message: error.message,
                    });
                }
            }
            let account = normalize_credentials(&credentials);
            if let Ok(mut table) = flows().lock() {
                table.remove(flow_id);
            }
            Ok(proto::LoginPoll::Done { account })
        }
        Err(error) => {
            if let Ok(mut table) = flows().lock() {
                table.remove(flow_id);
            }
            Ok(proto::LoginPoll::Failed {
                message: error.message,
            })
        }
    }
}

/// 取消一条流程（用户在界面上点了取消，或网关的总超时到了）。
pub(super) fn login_cancel(flow_id: &str) {
    if let Ok(mut table) = flows().lock() {
        table.remove(flow_id);
    }
}

/// 流程的发起时刻（清理用）。`CliLogin` 不存时刻，这里用一个近似：
/// 它的 `expires_at` 是上游给的截止秒数；缺失（0）时按「很久以前」处理，
/// 让清理逻辑优先摘掉它。
fn entry_started_at(flow: &CliLogin) -> i64 {
    let expires_at = flow.expires_at();
    if expires_at <= 0 {
        return 0;
    }
    // 上游给的是秒；转毫秒便于与 `now_ms` 比较
    expires_at.saturating_mul(1000)
}

/// 流程表条目的保留时长（超过它的一律当作孤儿清掉）。
const FLOW_KEEP_MS: i64 = 10 * 60_000;

/// 一次登录的凭证 → 插件协议的账号形态。
///
/// `secret` 是**插件侧**的落盘形状（与网关存量记录同键名，见 `store.rs` 的
/// 模块头），于是网关的 `flatten_builtin_secret` 与插件读到的键天然一致。
fn normalize_credentials(credentials: &super::credentials::ZcodeCredentials) -> proto::NormalizedAccount {
    let region = credentials.region;
    let secret = serde_json::json!({
        "accessToken": credentials.access_token,
        "jwt": credentials.jwt,
        "deviceMid": credentials.device_mid,
        "userId": credentials.user_id,
        super::PLAN_FIELD: super::PLAN_CODING,
        // 去重前缀（`identity_of` 读它）—— 与网关 `account_id_prefix` 同值
        "idPrefix": region.account_id_prefix(),
    });
    proto::NormalizedAccount {
        public: account_view(region, &secret),
        identity: identity_of(&secret),
        secret,
        desktop: false,
    }
}
