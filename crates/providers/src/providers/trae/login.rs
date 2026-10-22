//! Trae 网页登录：授权页 → **本机 loopback 回调** → 换凭据 → 收账号。
//!
//! ── 与网关那份的结构差别（迁移时改的）───────────────────────
//! 网关那版把「一轮登录」放进 `core::login::trae` 的待办表，任务句柄、
//! 取消语义、5 分钟超时全由网关持有；插件里没有那张表，所以改成：
//!
//! ```text
//!   login.start  → 绑 loopback 端口 + 造授权地址 + 起后台换证任务
//!   login.poll   → 只读进程内的 flow 状态（网关按 poll_interval_ms 来问）
//!   login.callback → 用户粘贴回调 URL：投递给同一个监听器，再等状态翻转
//!   login.cancel → 收监听器（释放端口）并清掉那一条 flow
//! ```
//!
//! ── 为什么报 `poll_interval_ms > 0`（本家明明是回调型）────────
//! 「轮询型」在协议里的含义是「**网关主动问插件要结果**」，与授权在哪里终结
//! 无关。本家的回调打在**插件自己**的 loopback 端口上（`127.0.0.1:<port>`），
//! 永远不经过网关的 `/api/session/login/callback` —— 如果报 `0`，网关会一直
//! 挂着等一个不会到来的回调，没有任何报错（最难查的一类失败）。所以这里报
//! 一个轮询间隔，让网关来问。
//!
//! ── 为什么换证要放后台任务 ──────────────────────────────────
//! `Session::complete()` 拿到回调后还要出网换证（ExchangeToken + GetUserInfo）。
//! 如果让 `login.poll` 现场 await 它，那次 RPC 一旦超时，回调已经从 channel
//! 里被消费掉、换证结果也丢了 —— 下一次 poll 永远等不到。所以后台任务持有
//! 「消费回调 → 换证 → 落状态」这条链，`login.poll` 只读状态。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic。

use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;

use agent2api_provider_protocol as proto;

use crate::logging;

use super::credentials::{Credential, DEFAULT_VARIANT};
use super::session::Session;

/// 流程 id 前缀（后面接时间戳，便于日志里认出是哪一轮）。
const FLOW_PREFIX: &str = "trae-web-";

/// 一轮登录在进程内的状态。
enum FlowState {
    /// 监听器已就位，还没收到回调。
    Waiting,
    /// 收到回调，正在换证（ExchangeToken + GetUserInfo）。
    Working,
    /// 换证完成。
    Done(Credential),
    /// 这一轮没成（用户取消 / 上游拒绝 / 换证失败）。
    Failed(String),
}

/// 表里的一条：会话与状态同生共死（`Session` 的 `Drop` 会释放端口）。
struct Entry {
    session: Arc<Session>,
    state: FlowState,
}

/// 进程内的登录表。
///
/// ── 为什么可以放进程内 ──────────────────────────────────────
/// 授权在几分钟内完成（网关侧超时 5 分钟），而插件进程在网关运行期间不会
/// 重启。进程真重启了，那次登录本来也就失败了（用户重新点一次即可）——
/// 与 qoder 的设备授权表同一处置。
fn flows() -> &'static Mutex<std::collections::HashMap<String, Entry>> {
    static FLOWS: OnceLock<Mutex<std::collections::HashMap<String, Entry>>> = OnceLock::new();
    FLOWS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// 在表上做一次操作（锁中毒时取回内部值继续 —— 一次登录不该让整个进程崩）。
fn with_flows<T>(action: impl FnOnce(&mut std::collections::HashMap<String, Entry>) -> T) -> T {
    match flows().lock() {
        Ok(mut table) => action(&mut table),
        Err(poisoned) => action(&mut poisoned.into_inner()),
    }
}

/// 发起一次网页登录。
///
/// `method` 不是 `browser` 时如实回绝：粘贴凭据走 `account.normalize`，
/// 不是这条链（回一个「请直接粘贴」比让它落到一个空授权地址上清楚）。
pub(super) async fn login_start(
    method: &str,
    _payload: &Value,
) -> Result<proto::LoginStart, String> {
    if method != "browser" {
        return Err(
            "Trae 的「填写凭证」请直接在账号页粘贴凭据；网页登录请点「网页登录」".to_string(),
        );
    }
    // 上一轮没收摊的先关掉：端口不释放的话，这一轮的回调可能落到上一轮的
    // 监听器上（两轮并存时回调落到哪个端口是不确定的，而授权码一次性）。
    close_all();
    let session = Arc::new(Session::begin(DEFAULT_VARIANT, None).await?);
    let flow_id = format!("{FLOW_PREFIX}{}", crate::http::now_ms());
    let auth_url = session.auth_url.clone();
    with_flows(|table| {
        table.insert(
            flow_id.clone(),
            Entry {
                session: Arc::clone(&session),
                state: FlowState::Waiting,
            },
        );
    });
    spawn_completion(flow_id.clone(), session);
    logging::log("[Login]", "发起 Trae 网页登录（等待浏览器回调…）");
    Ok(proto::LoginStart {
        flow: flow_id,
        url: auth_url,
        instructions: "在打开的页面里完成 Trae 授权，完成后会自动收下账号".to_string(),
        // 见模块头：网关必须来问（回调打在插件自己的 loopback 端口上）
        poll_interval_ms: 2_000,
        // 内嵌窗口：回调是 `http://127.0.0.1:<port>/authorize`，同一台机器上
        // 内嵌窗口能正常导航过去，而且登录态不外溢到系统浏览器
        external_browser: false,
        // 空 = 不限制：授权页会按用户选的登录方式继续跳不可穷举的主机
        // （与壳侧 `allowed_hosts` 里 trae 走 `None` 那条同口径）
        allowed_hosts: Vec::new(),
    })
}

/// 后台那半个登录：等回调 → 换证 → 把结果写回表。
fn spawn_completion(flow_id: String, session: Arc<Session>) {
    tokio::spawn(async move {
        let outcome = session.complete(None).await;
        with_flows(|table| {
            if let Some(entry) = table.get_mut(&flow_id) {
                entry.state = match outcome {
                    Ok(credential) => FlowState::Done(credential),
                    Err(error) => FlowState::Failed(error.message),
                };
            }
            // 表里已经没有这条（用户取消 / 网关超时后清理过）：什么都不做，
            // `Session` 的 Drop 会把端口还回去。
        });
    });
}

/// 轮询一次登录进度（网关按 `poll_interval_ms` 来问）。
pub(super) async fn login_poll(flow: &str) -> Result<proto::LoginPoll, String> {
    let state = with_flows(|table| match table.get(flow) {
        Some(entry) => match &entry.state {
            FlowState::Waiting => Some(proto::LoginPoll::Pending {
                hint: Some("等待你在浏览器里完成授权…".to_string()),
            }),
            FlowState::Working => Some(proto::LoginPoll::Pending {
                hint: Some("已收到授权，正在换取凭证…".to_string()),
            }),
            FlowState::Done(credential) => Some(done_poll(credential)),
            FlowState::Failed(message) => Some(proto::LoginPoll::Failed {
                message: message.clone(),
            }),
        },
        None => None,
    });
    match state {
        Some(result) => {
            // 终态一次性：Done / Failed 之后把这条移掉（端口随之释放）
            let terminal = matches!(
                result,
                proto::LoginPoll::Done { .. } | proto::LoginPoll::Failed { .. }
            );
            if terminal {
                with_flows(|table| table.remove(flow));
            }
            Ok(result)
        }
        // 表里没有这条：进程重启过，或者网关重发了一个已经收尾的 flow
        None => Ok(proto::LoginPoll::Failed {
            message: "这次登录流程已失效（插件进程重启过？），请重新发起".to_string(),
        }),
    }
}

/// 用户粘贴回来的回调 URL。
///
/// ── 这条为什么存在 ──────────────────────────────────────────
/// 授权页在本机跑时回调能直达 loopback；但如果用户是在**另一台机器**上完成
/// 授权（远程浏览器 / 容器里跑网关），回调就落不到这一台上 —— 那时用户会
/// 把浏览器地址栏里那条 URL 粘回来。两条路共用同一个监听器（投递后由后台
/// 任务照常换证），不复制第二套换证逻辑。
pub(super) async fn login_callback(
    callback: proto::LoginCallback,
) -> Result<proto::LoginPoll, String> {
    let query = callback_query(&callback);
    let session = with_flows(|table| {
        table.get(&callback.flow).map(|entry| {
            entry.session.submit_callback(&query)?;
            // 投递成功：状态可能已经是 Done（用户先粘过一次），读的人自己判
            Ok::<_, String>(Arc::clone(&entry.session))
        })
    });
    let Some(session) = session else {
        return Ok(proto::LoginPoll::Failed {
            message: "这次登录流程已失效，请重新发起".to_string(),
        });
    };
    let session = session?;
    // 投递成功 → 立刻标成「换证中」：后台任务从 channel 里取到 query 之后要
    // 出网换证（ExchangeToken + GetUserInfo），这段空窗期如果仍报「等待授权」，
    // 界面会显示成用户还没操作。
    with_flows(|table| {
        if let Some(entry) = table.get_mut(&callback.flow) {
            entry.state = FlowState::Working;
        }
    });
    // 后台任务已经在等回调，投递之后它会立刻进入换证。这里只等状态翻转，
    // 不自己再消费一次 channel（两个 consumer 抢同一个 receiver 只会让
    // 其中一边永远等不到）。
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let state = with_flows(|table| match table.get(&callback.flow) {
            Some(entry) => match &entry.state {
                FlowState::Done(credential) => Some(done_poll(credential)),
                FlowState::Failed(message) => Some(proto::LoginPoll::Failed {
                    message: message.clone(),
                }),
                _ => None,
            },
            None => Some(proto::LoginPoll::Failed {
                message: "这次登录流程已失效，请重新发起".to_string(),
            }),
        });
        if let Some(result) = state {
            let terminal = matches!(
                result,
                proto::LoginPoll::Done { .. } | proto::LoginPoll::Failed { .. }
            );
            if terminal {
                with_flows(|table| table.remove(&callback.flow));
            }
            return Ok(result);
        }
    }
    // 30 秒还没换完：如实说「还在换」，让网关按协议报错（回调型流程拿到
    // Pending 是协议错误，但比谎报失败好 —— 换证可能真的还在路上）。
    let _ = session;
    Ok(proto::LoginPoll::Pending {
        hint: Some("已收到授权，仍在换取凭证…".to_string()),
    })
}

/// 取消一轮登录：收监听器（释放端口）并清掉那一条。
pub(super) async fn login_cancel(flow: &str) -> Result<(), String> {
    let session = with_flows(|table| table.remove(flow).map(|entry| entry.session));
    if let Some(session) = session {
        // 显式 close：后台任务还握着另一份 Arc，只靠 Drop 会等 5 分钟超时才还端口
        session.close();
    }
    Ok(())
}

/// 从回调里取「投递给监听器的那串 query」。
///
/// 网关给的 `code` 在插件家是整条回调 URL（见 `plugin_host::rpc` 的
/// `exchange_login_code`：`url` 与 `code` 同值），而 `Callback::from_query`
/// 只认查询串 —— 所以带 `?` 的取问号之后那段，否则原样当 query。
fn callback_query(callback: &proto::LoginCallback) -> String {
    let raw = if callback.url.trim().is_empty() {
        callback.code.trim()
    } else {
        callback.url.trim()
    };
    match raw.split_once('?') {
        Some((_, query)) => query.to_string(),
        None => raw.to_string(),
    }
}

/// 换证成功 → `LoginPoll::Done`（谱系闸门失败时降级成 `Failed`）。
///
/// ── 为什么这里还要再判一次谱系 ──────────────────────────────
/// `accounts::into_account` 会拒 Intl 凭据（那条路落库后必然在转发时报
/// 一句难懂的 401）。登录链不能因此把一次已经完成的授权丢掉，但也**不能**
/// 把它落成账号 —— 所以翻成 `Failed` 并带上那句解释，用户看得到原因。
fn done_poll(credential: &Credential) -> proto::LoginPoll {
    match super::accounts::account_of(credential, "web", None).into_account() {
        Ok(account) => proto::LoginPoll::Done { account },
        Err(message) => proto::LoginPoll::Failed { message },
    }
}

/// 收掉表里所有轮次（发起新登录前调）。
fn close_all() {
    let drained: Vec<Arc<Session>> = with_flows(|table| {
        table
            .drain()
            .map(|(_, entry)| entry.session)
            .collect()
    });
    for session in drained {
        session.close();
    }
}
