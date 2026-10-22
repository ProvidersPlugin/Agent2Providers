//! CatPaw 的网页登录：**passport 会话 + 双通道收尾**。
//!
//! ── 协议（依据本机安装的 CatPaw 客户端，逐条已实测）──────────
//! ```text
//! ① GET  {gateway}/api/gateway/passport/login-config        （无需鉴权）
//!      → data.loginEntryUrl（实测 https://catpaw.meituan.com/api/gateway/passport/login-entry）
//! ② 把 loginEntryUrl 拼上三个 query 参数交给浏览器打开：
//!      ?state=<本地随机>&redirect=<loopback 地址>&sid=<本地随机>
//!    上游 302 → 美团 passport 登录 → settoken → login-callback?state=&sid=&redirect=
//! ③ 登录完成后，`login-callback` 页面把 `{token, state}` **POST 到 redirect 地址**
//!    （CSP 的 form-action 只允许 http://127.0.0.1:* 与 http://localhost:*）
//! ④ 校验 state 后把 token 落账号（token 就是 auth.json 的 auth.accessToken）
//! ```
//!
//! ── 两条通道，都要做 ────────────────────────────────────────
//! 客户端是 `Promise.race([loopback, poll-token])` —— **两条通道赛跑，谁先到用谁**。
//! 只做 loopback 不够：那次 POST 是**从公网页面（`catpaw.meituan.com`）发往本机
//! 127.0.0.1 的跨源请求**，浏览器会做私有网络检查（PNA）—— 官方客户端的
//! loopback 因此专门回了 `Access-Control-Allow-Private-Network: true`，而我们的
//! 监听器没有那个头，POST 就被浏览器挡在门外：用户看到上游的「登录成功」页、
//! 窗口一直不关、网关永远等不到凭证。
//!
//! 所以两条都做：
//!   - **loopback**（首选）：回调直接送到，毫秒级；
//!   - **poll-token**（兜底）：每 1 秒用自己生成的 `sid` 问一次上游，
//!     拿到就直接是 token。它不经过浏览器，因此不受任何 CORS / PNA 策略影响。
//!
//! ── 迁移时改了什么（网关 → 插件）───────────────────────────
//! | 网关 | 插件 |
//! |---|---|
//! | 回调挂在**网关自己的端口**上（`/api/session/login/catpaw-callback`） | 插件自绑 loopback 端口（`callback_server`） |
//! | 任务表在 `core::login`（`LoginTaskHandle`） | `login.rs` 的进程内表 |
//! | `poll_interval_ms` 由网关内部决定 | 本文件报 `> 0`（见下） |
//!
//! ── 为什么报 `poll_interval_ms > 0`（本家是回调型）────────────
//! 「轮询型」在协议里的含义是「**网关主动问插件要结果**」，与授权在哪里终结
//! 无关。本家的回调打在**插件自己**的 loopback 端口上，永远不经过网关的
//! `/api/session/login/callback` —— 报 `0` 的话网关会一直挂着等一个不会到来的
//! 回调，没有任何报错（最难查的一类失败）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic。

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};

use agent2api_provider_protocol as proto;

use crate::errors::GatewayError;
use crate::http::send_raw;
use crate::logging;

use super::callback_server::CallbackListener;
use super::PROVIDER_ID;

/// `login-config` 与 `current-user` 所在的基址。
///
/// 客户端按「租户 × 环境」选地址，external 租户的 prod 值就是这个（已实测）。
const GATEWAY_BASE: &str = "https://catx.nocode.cn";

/// 取登录入口地址的接口（客户端 `fetchLoginConfig` 的同一个地址）
const LOGIN_CONFIG_PATH: &str = "/api/gateway/passport/login-config";

/// 取会话信息的接口（登录成功后用它补 uid / 昵称）
const CURRENT_USER_PATH: &str = "/api/gateway/passport/current-user";

/// 轮询取件的接口（兜底通道，见模块头）。`?sid=<sid>` 即可，无需鉴权。
const POLL_TOKEN_PATH: &str = "/api/gateway/passport/poll-token";

/// HTTP 超时（客户端 `fetchLoginConfig` 用 5s，这里给同一个量级）
const REQUEST_TIMEOUT_MS: u64 = 10_000;

/// 单次轮询的超时（客户端 `pollForToken` 用 800ms —— 轮询口本身是轻查询，
/// 给太长会把 1 秒的间隔拖成实际 1 秒 + 超时时间）。
const POLL_TIMEOUT_MS: u64 = 5_000;

/// 轮询间隔（客户端同值）
const POLL_INTERVAL_MS: u64 = 1_000;

/// 流程 id 前缀
const FLOW_PREFIX: &str = "catpaw-web-";

/// 一轮登录在进程内的状态。
enum FlowState {
    /// 授权地址已给出，等回调 / 轮询。
    Waiting,
    /// 拿到 token，正在换账号资料。
    Working,
    /// 换好了。
    Done(Value),
    /// 这一轮没成。
    Failed(String),
}

/// 表里的一条：监听器（释放端口）+ sid（轮询用）+ 状态。
struct Entry {
    listener: Arc<CallbackListener>,
    /// 轮询兜底通道用的 sid。
    ///
    /// 表里留着它是为了让「网关重发同一个 flow 的 poll」也能看到这一轮的全部
    /// 信息（排障时读一眼就知道当初用的是哪个 sid）；轮询任务本身在
    /// `spawn_completion` 里拿到的是克隆，不依赖这个字段。
    #[allow(dead_code)]
    sid: String,
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

/// 发起一次 CatPaw 网页登录。
pub(super) async fn login_start(
    method: &str,
    _payload: &Value,
) -> Result<proto::LoginStart, String> {
    if method != "browser" {
        return Err(
            "CatPaw 的「填写凭据」请直接在账号页粘贴登录态；网页登录请点「网页登录」".to_string(),
        );
    }
    // 上一轮没收摊的先关掉：端口不释放的话，这一轮的回调可能落到上一轮的
    // 监听器上（授权码一次性，两轮并存等于让其中一轮必输）。
    close_all();
    let entry_url = fetch_login_entry().await?;
    let listener = Arc::new(CallbackListener::bind().await?);
    let redirect = format!(
        "http://127.0.0.1:{}{}",
        listener.port(),
        super::callback_server::CALLBACK_PATH
    );
    let state = random_hex()?;
    let sid = random_hex()?;
    let mut url = url::Url::parse(&entry_url)
        .map_err(|_| "CatPaw 登录入口地址无效".to_string())?;
    url.query_pairs_mut()
        .append_pair("state", &state)
        .append_pair("redirect", &redirect)
        .append_pair("sid", &sid);
    let auth_url = url.to_string();
    let flow_id = format!("{FLOW_PREFIX}{state}");
    with_flows(|table| {
        table.insert(
            flow_id.clone(),
            Entry {
                listener: Arc::clone(&listener),
                sid: sid.clone(),
                state: FlowState::Waiting,
            },
        );
    });
    spawn_completion(flow_id.clone(), Arc::clone(&listener), sid);
    logging::log("[Login]", "发起 CatPaw 网页登录（loopback 回调 + poll-token 双通道）");
    Ok(proto::LoginStart {
        flow: flow_id,
        url: auth_url,
        instructions: "在弹出的窗口里完成 CatPaw 登录，完成后会自动收下账号".to_string(),
        // 见模块头：回调打在插件自己的 loopback 端口上，网关必须来问
        poll_interval_ms: POLL_INTERVAL_MS,
        // 内嵌窗口：回调是 `http://127.0.0.1:<port>/catpaw/callback`，
        // 同一台机器上内嵌窗口能正常导航过去
        external_browser: false,
        // 空 = 不限制：passport 会按用户选的登录方式继续跳不可穷举的主机
        allowed_hosts: Vec::new(),
    })
}

/// 后台那半个登录：双通道赛跑，谁先到用谁。
///
/// ── 为什么是「赛跑」而不是「先等回调、超时再轮询」──────────────
/// 官方客户端就是 `Promise.race`：loopback 被 PNA 挡住时轮询是唯一出路，
/// 而轮询要 1 秒起步 —— 先等回调 5 分钟再轮询等于白等 5 分钟。两条同时起，
/// 谁先拿到 token 谁就落状态（`with_flows` 里判一次，后到的看到终态就退出）。
fn spawn_completion(flow_id: String, listener: Arc<CallbackListener>, sid: String) {
    tokio::spawn(async move {
        // 通道一：loopback 回调
        let callback_task = {
            let flow_id = flow_id.clone();
            let listener = Arc::clone(&listener);
            async move {
                let query = listener.next_callback().await;
                let token = query
                    .as_deref()
                    .map(|query| token_from_query(query))
                    .unwrap_or_default();
                if token.is_empty() {
                    return None;
                }
                Some((flow_id, token))
            }
        };
        // 通道二：上游 poll-token
        let poll_task = {
            let flow_id = flow_id.clone();
            let sid = sid.clone();
            async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
                    if is_terminal(&flow_id) {
                        return None;
                    }
                    if let Some(token) = poll_token(&sid).await {
                        return Some((flow_id, token));
                    }
                }
            }
        };
        let picked = tokio::select! {
            first = callback_task => first,
            first = poll_task => first,
        };
        let Some((flow_id, token)) = picked else {
            return;
        };
        // 换账号资料 → 落状态
        with_flows(|table| {
            if let Some(entry) = table.get_mut(&flow_id) {
                entry.state = FlowState::Working;
            }
        });
        let payload = login_session(&token).await;
        with_flows(|table| {
            if let Some(entry) = table.get_mut(&flow_id) {
                entry.state = match payload {
                    Ok(payload) => FlowState::Done(payload),
                    Err(error) => FlowState::Failed(error.message),
                };
            }
        });
        // 这一轮收摊：端口还回去（另一条通道已经没用）
        listener.close();
    });
}

/// 轮询一次登录进度。
pub(super) async fn login_poll(flow: &str) -> Result<proto::LoginPoll, String> {
    let state = with_flows(|table| match table.get(flow) {
        Some(entry) => Some(match &entry.state {
            FlowState::Waiting => proto::LoginPoll::Pending {
                hint: Some("等待你在浏览器里完成授权…".to_string()),
            },
            FlowState::Working => proto::LoginPoll::Pending {
                hint: Some("已收到凭证，正在读取账号资料…".to_string()),
            },
            FlowState::Done(payload) => done_poll(payload),
            FlowState::Failed(message) => proto::LoginPoll::Failed {
                message: message.clone(),
            },
        }),
        None => None,
    });
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
    Ok(())
}

/// 换证结果 → `LoginPoll`（归一化在 `accounts`，失败翻成 `Failed`）。
///
/// ── 为什么失败不直接报错 ────────────────────────────────────
/// 拿到 token 却归一化失败（例如 current-user 没给标识、token 里带分号），
/// 那是**这条账号不能落库**，不是「这一轮登录没成」—— 用 `Failed` 把原因
/// 带给用户，比让网关的 `login.poll` 报一个 502 清楚得多。
fn done_poll(payload: &Value) -> proto::LoginPoll {
    let input = proto::NormalizeInput {
        provider: PROVIDER_ID.to_string(),
        source: proto::NormalizeSource::Manual {
            payload: payload.clone(),
        },
        name: None,
        proxy: None,
    };
    match super::accounts::normalize(input) {
        Ok(account) => proto::LoginPoll::Done { account },
        Err(message) => proto::LoginPoll::Failed { message },
    }
}

/// 是不是已经到终态（轮询循环据此退出）。
fn is_terminal(flow: &str) -> bool {
    with_flows(|table| match table.get(flow) {
        Some(entry) => matches!(
            entry.state,
            FlowState::Done(_) | FlowState::Failed(_)
        ),
        // 表里没有了（用户取消 / 网关超时后清理过）→ 也算终态
        None => true,
    })
}

/// 从回调 query 里取 `token`（上游 POST 的是表单，这里收到的是查询串形态）。
fn token_from_query(query: &str) -> String {
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key != "token" {
            continue;
        }
        return percent_decode(value).trim().to_string();
    }
    String::new()
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

/// 轮询上游取 token（兜底通道）。
///
/// 契约为客户端 `pollForToken` 的同一份（已实测）：
///   - `GET {base}/api/gateway/passport/poll-token?sid=<sid>`
///   - 未就绪 → `{code:0, data:null}`，继续等；
///   - 就绪 → `data` **直接是 token 字符串**（不再包一层结构）；
///   - 网络错误单拍忽略（客户端也是 `try/catch` 后继续）。
async fn poll_token(sid: &str) -> Option<String> {
    let url = format!("{GATEWAY_BASE}{POLL_TOKEN_PATH}?sid={sid}");
    let headers = vec![("Accept".to_string(), "application/json".to_string())];
    let response = send_raw("GET", &url, None, &headers, None, Some(POLL_TIMEOUT_MS))
        .await
        .ok()?;
    response
        .payload
        .as_ref()
        .and_then(|payload| payload.get("data"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// 取登录入口地址（`login-config`，无需鉴权）。
async fn fetch_login_entry() -> Result<String, String> {
    let url = format!("{GATEWAY_BASE}{LOGIN_CONFIG_PATH}");
    let headers = vec![("Accept".to_string(), "application/json".to_string())];
    let response = send_raw("GET", &url, None, &headers, None, Some(REQUEST_TIMEOUT_MS))
        .await
        .map_err(|error| format!("无法连接 CatPaw 登录服务: {error}"))?;
    let payload = response.payload.unwrap_or(Value::Null);
    payload
        .get("data")
        .and_then(|data| data.get("loginEntryUrl"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "CatPaw 登录服务未返回登录入口地址".to_string())
}

/// 32 位小写 hex 随机串（客户端用 `crypto.getRandomValues(16)` 生成 state/sid，
/// 这里沿用同一强度与形态）。
fn random_hex() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|_| "无法生成安全的登录随机串".to_string())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// 把拿到的 token 加工成 `accounts::normalize` 认得的 payload
/// （顺带问一次 `current-user` 补 uid / 昵称）。
///
/// `current-user` 失败**不算登录失败**：token 本身已经到手，uid 缺失时
/// `accounts` 会按「缺 uid 或 loginName」拒 —— 所以这里必须至少带一个标识：
/// 拿不到 userId 时退回 token 尾（那条路只会造出一个展示名难看的账号，
/// 但不会把一次成功登录判死）。
async fn login_session(token: &str) -> Result<Value, GatewayError> {
    let headers = vec![
        ("Accept".to_string(), "application/json".to_string()),
        ("X-Auth-Token".to_string(), token.to_string()),
    ];
    let mut auth = serde_json::Map::new();
    auth.insert("loginType".to_string(), Value::String("passport".to_string()));
    auth.insert("accessToken".to_string(), Value::String(token.to_string()));
    auth.insert("tokenType".to_string(), Value::String("Bearer".to_string()));
    let mut account = serde_json::Map::new();
    let url = format!("{GATEWAY_BASE}{CURRENT_USER_PATH}");
    if let Ok(response) = send_raw("GET", &url, None, &headers, None, Some(REQUEST_TIMEOUT_MS)).await {
        if let Some(data) = response.payload.as_ref().and_then(|value| value.get("data")) {
            // userId 可能是数字或字符串，两种都收（客户端也做 String() 转换）
            let uid = data
                .get("userId")
                .map(|value| match value {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            let name = data
                .get("userName")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !uid.is_empty() && uid != "null" {
                account.insert("uid".to_string(), Value::String(uid));
            }
            if !name.is_empty() {
                account.insert("loginName".to_string(), Value::String(name.clone()));
                account.insert("name".to_string(), Value::String(name));
            }
        }
    }
    if !account.contains_key("uid") && !account.contains_key("loginName") {
        // current-user 没给标识：用令牌尾兜底（`accounts` 要求 uid 或 loginName
        // 至少有一个，否则这条账号无法寻址）
        account.insert(
            "loginName".to_string(),
            Value::String(format!("catpaw-{}", crate::store::token_tail(token))),
        );
    }
    Ok(json!({ "auth": Value::Object(auth), "account": Value::Object(account) }))
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
