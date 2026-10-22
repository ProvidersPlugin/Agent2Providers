//! Accio 网页登录：**loopback 回调 + PKCE**，进程内的 pending 表。
//!
//! ── 协议（依据桌面端实现 `AuthService`）────────────────────────
//! ```text
//! ① 生成 code_verifier（随机）/ code_challenge（SHA-256 → base64url）
//! ② 打开 {loginBase}/login?return_url=<本机回调>&state=<state>
//!        &code_challenge=<challenge>&code_challenge_method=S256&client_id=accio-work
//! ③ 用户登录完成 → 登录页把 ?code=…&state=… 拼到 return_url 上跳回来
//! ④ POST {gw}/api/oauth/token  {code, codeVerifier, clientId, redirectUri}
//!        → {accessToken, refreshToken, expiresAt}
//! ```
//!
//! ── 迁移时改了什么（网关 → 插件）───────────────────────────
//! | 网关 | 插件 |
//! |---|---|
//! | 回调挂在**网关端口**（`/auth/callback-accio`） | 插件自绑 loopback 端口（`callback_server`） |
//! | 端口由 `ServerState::bootstrap` 写进 `LOOPBACK_PORT` | `CallbackListener::bind()` 现取现用 |
//! | 收尾由网关的 `submit_login_callback` 调 `exchange_login_code` | 后台任务自己等回调、自己换码 |
//! | 任务表在 `core::login` | `login.rs` 的进程内表 |
//!
//! ── 为什么报 `poll_interval_ms > 0`（本家是回调型）────────────
//! 「轮询型」在协议里的含义是「**网关主动问插件要结果**」，与授权在哪里终结
//! 无关。本家的回调打在**插件自己**的 loopback 端口上，永远不经过网关的
//! `/api/session/login/callback` —— 报 `0` 的话网关会一直挂着等一个不会到来的
//! 回调，没有任何报错。
//!
//! ── 为什么要 PKCE 与 state（而不是只带一个 code）──────────────
//! 回调落在本机 HTTP 端口上，任何本机进程都能发出一次 GET。两道闸门各挡一类：
//!   - **state**：`← 这次回调是不是本进程刚发起的那一轮`。逐字比对，不匹配就
//!     拒绝 —— 否则别人可以拿自己那次的 code 把凭证写进你的账号库；
//!   - **code_verifier**：规范给的防换码能力（授权码被截获也换不出 token）。
//! 两者都随 pending 表进出：**只在内存**，不落盘（进程重启等于那次登录作废）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic。

use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;

use agent2api_provider_protocol as proto;

use crate::logging;

use super::callback_server::CallbackListener;
use super::endpoints::Region;
use super::oauth::{self, PendingLogin};

/// 流程 id 前缀
const FLOW_PREFIX: &str = "accio-web-";

/// 一轮登录在进程内的状态。
enum FlowState {
    /// 授权地址已给出，等回调。
    Waiting,
    /// 拿到 code，正在换令牌。
    Working,
    /// 换好了（存的是 `Credentials` 的落库形态）。
    Done(Value),
    /// 这一轮没成。
    Failed(String),
}

struct Entry {
    listener: Arc<CallbackListener>,
    state: FlowState,
}

fn flows() -> &'static Mutex<std::collections::HashMap<String, Entry>> {
    static FLOWS: OnceLock<Mutex<std::collections::HashMap<String, Entry>>> = OnceLock::new();
    FLOWS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn with_flows<T>(action: impl FnOnce(&mut std::collections::HashMap<String, Entry>) -> T) -> T {
    match flows().lock() {
        Ok(mut table) => action(&mut table),
        Err(poisoned) => action(&mut poisoned.into_inner()),
    }
}

/// 发起一次 Accio 网页登录。
pub(super) async fn login_start(
    region: Region,
    method: &str,
    _payload: &Value,
) -> Result<proto::LoginStart, String> {
    if method != "browser" {
        return Err(format!(
            "Accio {}的「填写凭据」请直接在账号页粘贴凭证；网页登录请点「网页登录」",
            region.label()
        ));
    }
    // 上一轮没收摊的先关掉：端口不释放的话，这一轮的回调可能落到上一轮的
    // 监听器上（授权码一次性，两轮并存等于让其中一轮必输）。
    close_all();
    let listener = Arc::new(CallbackListener::bind().await?);
    let return_url = format!(
        "http://127.0.0.1:{}{}",
        listener.port(),
        super::callback_server::PATH
    );
    let (auth_url, state) = oauth::begin_login(region, &return_url);
    let flow_id = format!("{FLOW_PREFIX}{state}");
    with_flows(|table| {
        table.insert(
            flow_id.clone(),
            Entry {
                listener: Arc::clone(&listener),
                state: FlowState::Waiting,
            },
        );
    });
    spawn_completion(flow_id.clone(), Arc::clone(&listener), region);
    logging::log(
        "[Login]",
        &format!("发起 Accio {}网页登录（等待浏览器回调…）", region.label()),
    );
    Ok(proto::LoginStart {
        flow: flow_id,
        url: auth_url,
        instructions: format!(
            "在打开的页面里完成 Accio {} 的授权，完成后会自动收下账号",
            region.label()
        ),
        // 见模块头：回调打在插件自己的 loopback 端口上，网关必须来问
        poll_interval_ms: 2_000,
        // 内嵌窗口：回调是 `http://127.0.0.1:<port>/auth/callback-accio`，
        // 同一台机器上内嵌窗口能正常导航过去
        external_browser: false,
        // 空 = 不限制：授权页会按用户选的登录方式继续跳不可穷举的主机
        allowed_hosts: Vec::new(),
    })
}

/// 后台那半个登录：等回调 → 换码 → 落状态。
fn spawn_completion(flow_id: String, listener: Arc<CallbackListener>, region: Region) {
    tokio::spawn(async move {
        let Some(query) = listener.next_callback().await else {
            return;
        };
        let pending = with_flows(|table| match table.get(&flow_id) {
            Some(entry) => {
                let _ = &entry.state;
                Some(())
            }
            None => None,
        });
        if pending.is_none() {
            // 表里已经没有这条（用户取消 / 网关超时后清理过）
            return;
        }
        with_flows(|table| {
            if let Some(entry) = table.get_mut(&flow_id) {
                entry.state = FlowState::Working;
            }
        });
        let outcome = finish(&flow_id, region, &query).await;
        with_flows(|table| {
            if let Some(entry) = table.get_mut(&flow_id) {
                entry.state = match outcome {
                    Ok(credentials) => FlowState::Done(credentials.to_value()),
                    Err(message) => FlowState::Failed(message),
                };
            }
        });
        // 这一轮收摊：端口还回去
        listener.close();
    });
}

/// 用回调 query 换令牌（`oauth::take_pending` 取那一轮的 verifier）。
async fn finish(
    flow_id: &str,
    region: Region,
    query: &str,
) -> Result<super::credentials::Credentials, String> {
    // flow id 是 `accio-web-<state>`，state 就是后面那段
    let state = flow_id
        .strip_prefix(FLOW_PREFIX)
        .unwrap_or(flow_id)
        .to_string();
    let params: std::collections::HashMap<String, String> = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (percent_decode(key), percent_decode(value)))
        .collect();
    let (code, callback_state) = oauth::parse_callback(&params).map_err(|error| error.message)?;
    if callback_state != state {
        return Err("回调里的 state 与本次登录不一致，已拒绝".to_string());
    }
    let Some(pending) = oauth::take_pending(&state) else {
        return Err("这次登录已取消或已过期，请重新发起".to_string());
    };
    let PendingLogin {
        region: pending_region,
        verifier,
        redirect_uri,
        ..
    } = pending;
    // 用 pending 里的地区（它才是发起时的那一站；回调地址可能被改过）
    let _ = region;
    oauth::exchange_code(pending_region, &code, &verifier, &redirect_uri, None)
        .await
        .map_err(|error| error.message)
}

/// 轮询一次登录进度。
pub(super) async fn login_poll(
    region: Region,
    flow: &str,
) -> Result<proto::LoginPoll, String> {
    let state = with_flows(|table| match table.get(flow) {
        Some(entry) => Some(match &entry.state {
            FlowState::Waiting => proto::LoginPoll::Pending {
                hint: Some("等待你在浏览器里完成授权…".to_string()),
            },
            FlowState::Working => proto::LoginPoll::Pending {
                hint: Some("已收到授权码，正在换取令牌…".to_string()),
            },
            FlowState::Done(payload) => {
                // 落库形态 → 归一化（`accounts::normalize` 是 async 的，
                // 但它只查一次资料；这里已经有完整凭证，走 `account_of` 那条
                // 同步路即可 —— 资料在换码时已经查过了）
                match super::credentials::Credentials::from_payload(payload) {
                    Ok(credentials) => proto::LoginPoll::Done {
                        account: super::accounts::account_of(&credentials, "web", None),
                    },
                    Err(error) => proto::LoginPoll::Failed {
                        message: error.message,
                    },
                }
            }
            FlowState::Failed(message) => proto::LoginPoll::Failed {
                message: message.clone(),
            },
        }),
        None => None,
    });
    let _ = region;
    match state {
        Some(result) => {
            let terminal = matches!(
                result,
                proto::LoginPoll::Done { .. } | proto::LoginPoll::Failed { .. }
            );
            if terminal {
                with_flows(|table| table.remove(flow));
            }
            Ok(result)
        }
        None => Ok(proto::LoginPoll::Failed {
            message: "这次登录流程已失效（插件进程重启过？），请重新发起".to_string(),
        }),
    }
}

/// 取消一轮登录：收监听器（释放端口）并清掉那一条。
pub(super) async fn login_cancel(flow: &str) -> Result<(), String> {
    let listener = with_flows(|table| table.remove(flow).map(|entry| entry.listener));
    if let Some(listener) = listener {
        listener.close();
    }
    if let Some(state) = flow.strip_prefix(FLOW_PREFIX) {
        oauth::drop_pending(state);
    }
    Ok(())
}

/// 收掉表里所有轮次（发起新登录前调）。
fn close_all() {
    let drained: Vec<Arc<CallbackListener>> = with_flows(|table| {
        table.drain().map(|(_, entry)| entry.listener).collect()
    });
    for listener in drained {
        listener.close();
    }
}

/// `application/x-www-form-urlencoded` 的解码（`+` = 空格，`%XX` = 字节）。
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = &value[index + 1..index + 3];
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
