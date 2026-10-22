//! AutoClaw 网页登录的**流程编排**：借登记端口、等浏览器回调、换码，把结果放进
//! 流程表，网关按 `pollIntervalMs` 来问。
//!
//! ── 为什么是轮询型而不是回调型 ──────────────────────────────
//! 协议里 `poll_interval_ms > 0` 的含义是「**网关主动问插件要结果**」，与授权在
//! 哪里终结无关。本家的回调打在**插件自己**的 loopback 端口上，永远不经过网关
//! 的 `/api/session/login/callback` —— 报 `0` 的话网关会一直挂着等一个不会到来
//! 的回调，没有任何报错。这与 Accio / Trae / CatPaw 是同一处置。
//!
//! ── 两条入口，一张表 ────────────────────────────────────────
//! ```text
//!   浏览器直达   授权页 302 → 插件占的登记端口 → 本文件的监听器
//!   远程面板     用户把地址栏整条 URL 粘回 → login.oauth-callback 管理动作
//! ```
//! 两条都只投递「变体 + 查询串」，换码逻辑只有一份（[`finish`]）。
//!
//! ── 为什么发起走管理动作而不是 `login.start` ──────────────────
//! 取授权地址（第 ② 步）**需要前端跑完阿里云 SDK 得到的 `ali_captcha_verify_param`**，
//! 而 `login.start` 在网关侧是固定空 payload（见 `RpcAdapter::login_start_async`）。
//! 所以第 ② 步走 `login.oauth-start`，由网关把前端的验证码参数原样转发进来。
//!
//! ── 端口一个都抢不到时 ──────────────────────────────────────
//! 如实报错并让用户先退出 AutoClaw 桌面客户端：Zai 的 redirect_uri 白名单只认
//! 那四个端口，抢不到就没有任何可用的回调落点（网关侧那条「内嵌窗口改道」的
//! 兜底已随实现一起移除 —— 网关不再持有任何 AutoClaw 的回调路由）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 本 crate 编译期禁止 unwrap/expect/panic。

use std::sync::{Arc, Mutex, OnceLock};

use agent2api_provider_protocol as proto;
use serde_json::{json, Value};

use crate::errors::GatewayError;
use crate::logging;

use super::callback_server::CallbackListener;
use super::credentials;
use super::oauth::{self, Vendor};
use super::region::Region;

/// 流程 id 前缀（便于日志与排障时指认）
const FLOW_PREFIX: &str = "autoclaw-web-";

/// 轮询间隔：回调是本机的（延迟极低），1 秒足够且不会让界面觉得卡。
const POLL_INTERVAL_MS: u64 = 1_000;

/// 一轮登录在进程内的状态。
enum FlowState {
    /// 授权地址已给出，等浏览器回调。
    Waiting,
    /// 拿到 code，正在换码。
    Working,
    /// 换好了（存的是扁平凭证，落库时再 `account_from_credentials`）。
    Done(Value),
    /// 这一轮没成。
    Failed(String),
}

struct Entry {
    region: Region,
    vendor: Vendor,
    navigate_uri: String,
    device_id: String,
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

/// 收掉所有未完成的流程（新一轮发起前调用）：端口不释放的话，这一轮的回调可能
/// 落到上一轮的监听器上（授权码一次性，两轮并存等于让其中一轮必输）。
fn close_all() {
    let entries: Vec<Arc<CallbackListener>> =
        with_flows(|table| table.drain().map(|(_, entry)| entry.listener).collect());
    for listener in entries {
        listener.close();
    }
}

/// 发起一次网页登录：借登记端口 → 取授权地址 → 登记流程 → 起后台收尾。
///
/// 返回 `(flow, auth_url)`。
pub(super) async fn start_oauth(
    region: Region,
    vendor: Vendor,
    captcha_verify_param: &str,
) -> Result<(String, String), GatewayError> {
    close_all();
    let device_id = oauth::new_oauth_device_id();
    let (sender, inbox) = tokio::sync::mpsc::unbounded_channel();
    let listener = CallbackListener::bind(sender).await.map_err(|reason| {
        logging::log("[Login]", &reason);
        GatewayError::with_status(
            400,
            format!("{reason}；请先退出 AutoClaw 桌面客户端后重试"),
        )
    })?;
    let base = format!("http://localhost:{}", listener.port());
    let navigate = oauth::navigate_uri(&base, vendor);
    let auth_url =
        oauth::request_oauth_url(region, vendor, &navigate, captcha_verify_param, &device_id).await?;
    let state = random_hex()?;
    let flow = format!("{FLOW_PREFIX}{state}");
    let listener = Arc::new(listener);
    with_flows(|table| {
        table.insert(
            flow.clone(),
            Entry {
                region,
                vendor,
                navigate_uri: navigate.clone(),
                device_id,
                listener: Arc::clone(&listener),
                state: FlowState::Waiting,
            },
        );
    });
    spawn_completion(flow.clone(), listener, inbox);
    logging::log(
        "[Login]",
        &format!(
            "发起 AutoClaw {} {}网页登录（等浏览器回调 {navigate}）",
            region.label(),
            vendor.label()
        ),
    );
    Ok((flow, auth_url))
}

/// 后台那半个登录：等回调 → 换码 → 落状态。
fn spawn_completion(
    flow: String,
    listener: Arc<CallbackListener>,
    mut inbox: tokio::sync::mpsc::UnboundedReceiver<String>,
) {
    tokio::spawn(async move {
        let Some(payload) = inbox.recv().await else {
            return;
        };
        let (region, vendor, navigate_uri, device_id) = {
            let found = with_flows(|table| {
                table.get(&flow).map(|entry| {
                    (
                        entry.region,
                        entry.vendor,
                        entry.navigate_uri.clone(),
                        entry.device_id.clone(),
                    )
                })
            });
            let Some(found) = found else {
                // 表里已经没有这条（用户取消 / 网关超时后清理过）
                return;
            };
            found
        };
        let (callback_vendor, query) = split_delivery(&payload);
        // 粘贴进来的回调带变体，必须与这一轮一致；不一致说明用户贴错了
        if let Some(callback_vendor) = callback_vendor {
            if callback_vendor != vendor.id() {
                finish_flow(
                    &flow,
                    Err(format!(
                        "回调属于 {}，与本次 {} 登录不一致，请重新发起",
                        callback_vendor,
                        vendor.label()
                    )),
                );
                listener.close();
                return;
            }
        }
        with_flows(|table| {
            if let Some(entry) = table.get_mut(&flow) {
                entry.state = FlowState::Working;
            }
        });
        let outcome =
            finish(region, vendor, &navigate_uri, &device_id, &query).await;
        finish_flow(&flow, outcome.map_err(|error| error.message));
        listener.close();
    });
}

/// 投递形态：`{vendor}|{query}`。裸查询串（用户粘贴）没有前缀。
fn split_delivery(payload: &str) -> (Option<String>, String) {
    match payload.split_once('|') {
        Some((vendor, query)) => (Some(vendor.to_string()), query.to_string()),
        None => (None, payload.to_string()),
    }
}

/// 用回调 query 换码，产出可落库的扁平凭证。
async fn finish(
    region: Region,
    vendor: Vendor,
    navigate_uri: &str,
    device_id: &str,
    query: &str,
) -> Result<Value, GatewayError> {
    let params: std::collections::HashMap<String, String> = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (percent_decode(key), percent_decode(value)))
        .collect();
    if let Some(error) = params.get("error").filter(|value| !value.trim().is_empty()) {
        return Err(GatewayError::with_status(
            400,
            format!("授权被拒绝（{error}）"),
        ));
    }
    let code = params.get("code").map(String::as_str).unwrap_or("").trim();
    if code.is_empty() {
        return Err(GatewayError::with_status(400, "回调没有携带授权码"));
    }
    // 查询串里的 state 是**上游生成的**那个（换码要回传它），不是我们的 flow。
    // 粘贴路径可能只剩 code（网关的 `login.callback` 只抽出 code 参数），
    // 此时传空串 —— 官方客户端同样是 `searchParams.get("state") || ""`。
    let upstream_state = params.get("state").map(String::as_str).unwrap_or("");
    let mut credentials = oauth::exchange_code(
        region,
        vendor,
        code,
        upstream_state.trim(),
        navigate_uri,
        device_id,
    )
    .await?;
    // 邮箱：换回来的 token 里没有它（JWT 只有 user_id / device_id / guid），
    // 而账号在列表上得认得出是谁。best-effort：查不到照常建账号。
    if let Ok(parsed) = credentials::credentials_from_record(&credentials, region) {
        let email = super::profile::email_or_empty(&parsed, "网页登录").await;
        if !email.is_empty() {
            if let Some(object) = credentials.as_object_mut() {
                object.insert("email".to_string(), Value::String(email));
            }
        }
    }
    Ok(credentials)
}

fn finish_flow(flow: &str, outcome: Result<Value, String>) {
    with_flows(|table| {
        if let Some(entry) = table.get_mut(flow) {
            entry.state = match outcome {
                Ok(credentials) => FlowState::Done(credentials),
                Err(message) => FlowState::Failed(message),
            };
        }
    });
}

/// 轮询一次登录进度。
pub(super) async fn login_poll(flow: &str) -> Result<proto::LoginPoll, String> {
    let result = with_flows(|table| match table.get(flow) {
        Some(entry) => Some(match &entry.state {
            FlowState::Waiting => proto::LoginPoll::Pending {
                hint: Some("等待你在浏览器里完成授权…".to_string()),
            },
            FlowState::Working => proto::LoginPoll::Pending {
                hint: Some("已收到授权码，正在换取登录态…".to_string()),
            },
            FlowState::Done(credentials) => {
                match super::login::account_from_credentials(entry.region, credentials.clone()) {
                    Ok(account) => proto::LoginPoll::Done { account },
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
    match result {
        Some(result) => {
            let terminal = matches!(
                result,
                proto::LoginPoll::Done { .. } | proto::LoginPoll::Failed { .. }
            );
            if terminal {
                let listener = with_flows(|table| table.remove(flow).map(|entry| entry.listener));
                if let Some(listener) = listener {
                    listener.close();
                }
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

/// 用户手工粘贴回调地址（远程面板形态：浏览器到不了插件所在机器的 loopback）。
///
/// 接受三种输入：完整回调 URL、裸查询串、裸授权码 —— 网关的 `login.callback`
/// 只抽出 `code` 参数给我们，因此第三种必须支持（变体与 `navigate_uri` 都从
/// 流程表里取，不需要 URL 提供）。
pub(super) async fn submit_callback(flow: &str, payload: &str) -> Result<(), String> {
    let entry = with_flows(|table| {
        table
            .get(flow)
            .filter(|entry| matches!(entry.state, FlowState::Waiting))
            .map(|entry| (entry.vendor, entry.listener.clone()))
    });
    let Some((vendor, listener)) = entry else {
        return Err("这次登录已取消或已过期，请重新发起".to_string());
    };
    let text = payload.trim();
    if text.is_empty() {
        return Err("回调地址为空".to_string());
    }
    let (vendor_from_url, query) = if let Some(rest) = text.strip_prefix(oauth::CALLBACK_PATH_PREFIX) {
        match rest.split_once('?') {
            Some((vendor, query)) => (Some(vendor.to_string()), query.to_string()),
            None => (Some(rest.to_string()), String::new()),
        }
    } else if text.contains("code=") {
        (None, text.trim_start_matches('?').to_string())
    } else {
        // 裸授权码
        (None, format!("code={}", text))
    };
    if let Some(vendor_from_url) = vendor_from_url {
        if !vendor_from_url.is_empty() && vendor_from_url != vendor.id() {
            return Err(format!(
                "回调属于 {vendor_from_url}，与本次 {} 登录不一致",
                vendor.label()
            ));
        }
    }
    let params: std::collections::HashMap<String, String> = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    let has_code = params
        .get("code")
        .is_some_and(|value| !value.trim().is_empty());
    let has_error = params
        .get("error")
        .is_some_and(|value| !value.trim().is_empty());
    if !has_code && !has_error {
        return Err("回调地址没有携带 AutoClaw 授权码".to_string());
    }
    listener
        .deliver(&format!("{}|{query}", vendor.id()))
        .map_err(|_| "AutoClaw 登录任务已结束，请重新发起".to_string())
}

/// 32 位小写 hex 随机串（一次性流程 id 用；安全用途，必须用密码学随机源）。
fn random_hex() -> Result<String, GatewayError> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| GatewayError::with_status(502, "无法生成安全的登录随机串"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// 百分号解码（`+` 在查询串里按空格处理）。
fn percent_decode(input: &str) -> String {
    let mut bytes: Vec<u8> = Vec::with_capacity(input.len());
    let raw = input.as_bytes();
    let mut index = 0;
    while index < raw.len() {
        match raw[index] {
            b'%' if index + 2 < raw.len() => {
                let hex = &input[index + 1..index + 3];
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        bytes.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        bytes.push(raw[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                bytes.push(b' ');
                index += 1;
            }
            byte => {
                bytes.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// 供管理动作复用：把 `{state, authUrl, pollIntervalMs}` 组装成网关要的形状。
///
/// `pollIntervalMs` 必须给：这条链的回调落在**插件自己**的登记端口上，网关不会
/// 收到任何 `/api/session/login/callback` —— 只报地址不报间隔，界面就会一直挂着
/// 等一个永远不会到来的跳转（没有任何报错，是最难查的一类失败）。
pub(super) fn start_payload(flow: &str, auth_url: &str) -> Value {
    json!({
        "state": flow,
        "authUrl": auth_url,
        "pollIntervalMs": POLL_INTERVAL_MS,
    })
}
