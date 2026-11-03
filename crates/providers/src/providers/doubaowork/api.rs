use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{PluginCtx, StoreProxy};
use reqwest::header;
use serde_json::{json, Value};

use super::PROVIDER_ID;
use crate::errors::GatewayError;

pub const BASE_URL: &str = "https://www.doubao.com";
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36";
// 上游按 pc_version 下发模型菜单；只有 runtime_version 时会成功返回空菜单。
const DEFAULT_CLIENT_VERSION: &str = "2.33.5";

pub fn client(
    proxy: Option<&proto::WireProxy>,
    gateway_proxy: &str,
    stream: bool,
) -> Result<reqwest::Client, GatewayError> {
    type Key = (String, Option<crate::timeouts::Timeouts>);
    static CLIENTS: OnceLock<Mutex<HashMap<Key, reqwest::Client>>> = OnceLock::new();
    let proxy = proxy
        .map(proto::WireProxy::to_url)
        .unwrap_or_else(|| gateway_proxy.trim().to_string());
    let timeouts = stream.then(crate::timeouts::current);
    let key = (proxy.clone(), timeouts);
    let clients = CLIENTS.get_or_init(Mutex::default);
    if let Some(client) = clients
        .lock()
        .ok()
        .and_then(|clients| clients.get(&key).cloned())
    {
        return Ok(client);
    }
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(15));
    builder = match timeouts {
        Some(timeouts) => builder
            .connect_timeout(Duration::from_millis(timeouts.connect_ms()))
            .read_timeout(Duration::from_millis(timeouts.read_timeout_backstop_ms())),
        None => builder.timeout(Duration::from_secs(30)),
    };
    if !proxy.is_empty() && proxy != "direct" {
        let proxy = reqwest::Proxy::all(&proxy)
            .map_err(|_| GatewayError::with_status(400, "豆包工作代理地址无效"))?;
        builder = builder.proxy(proxy);
    }
    let client = builder
        .build()
        .map_err(|_| GatewayError::with_status(502, "无法创建豆包工作出网连接"))?;
    if let Ok(mut clients) = clients.lock() {
        if clients.len() >= 128 {
            clients.clear();
        }
        clients.insert(key, client.clone());
    }
    Ok(client)
}

pub fn number(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.trim().parse().ok())
}

pub fn identifier(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_u64().map(|id| id.to_string()))
        .filter(|id| !id.trim().is_empty())
}

pub fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

pub fn headers(credentials: &Value) -> Result<Vec<(String, String)>, GatewayError> {
    let cookie = super::accounts::cookie_header(credentials)?;
    Ok(vec![
        (header::COOKIE.as_str().into(), cookie),
        (header::ORIGIN.as_str().into(), BASE_URL.into()),
        (header::REFERER.as_str().into(), format!("{BASE_URL}/chat/")),
        (
            header::USER_AGENT.as_str().into(),
            credentials["userAgent"]
                .as_str()
                .unwrap_or(USER_AGENT)
                .into(),
        ),
        ("Agw-Js-Conv".into(), "str".into()),
    ])
}

/// 组装一次上游请求的完整地址（含查询串）。
///
/// ── 为什么单独拆出来 ────────────────────────────────────────
/// Images 扩展只交「地址 + 请求头 + 请求体」，不经 `reqwest` 的查询串编码，
/// 但它必须用**同一份**查询参数：豆包风控认的是「这个账号在这台受信设备上」
/// （见 [`super::client`] 模块头），少了 `device_id` / `fp` 一定会被人机验证
/// 拦下 —— 生图走的就是同一个 `/chat/completion`，没有例外。所以两边共用本
/// 函数，编码按 [`encode`] 的 RFC3986 未保留字符集自己来。
pub fn endpoint(credentials: &Value, path: &str) -> Result<String, GatewayError> {
    let client_version = credentials["clientVersion"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_CLIENT_VERSION);
    let mut query = vec![
        ("aid", "1044603".to_string()),
        ("real_aid", "1044603".into()),
        ("device_platform", "web".into()),
        ("version_code", "20800".into()),
        ("samantha_web", "1".into()),
        ("web_platform", "desktop".into()),
        ("doubao_device_platform", "desktop".into()),
        ("client_platform", "pc_client".into()),
        ("pkg_type", "release_version".into()),
        ("runtime", "web".into()),
        ("runtime_version", "3.42.2".into()),
        ("pc_version", client_version.to_string()),
        ("doubao_pc_version", client_version.to_string()),
        ("use-olympus-account", "1".into()),
        ("language", "zh".into()),
        ("region", "CN".into()),
        ("sys_region", "CN".into()),
    ];
    // 设备号：先用账号自己的（网页登录态里可能带 `device_id` Cookie），没有
    // 就兜底读本机豆包工作客户端的 —— 风控认的是「这个账号在这台受信设备上」，
    // 不读客户端的设备号，转发一定会被人机验证拦下（见 `client` 模块头的实测）。
    let device = credentials["deviceId"]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(super::client::device_id);
    for (wire, field) in [
        ("web_id", "webId"),
        ("chromium_version", "chromiumVersion"),
        ("channel", "channel"),
    ] {
        if let Some(value) = credentials[field]
            .as_str()
            .filter(|value| !value.is_empty())
        {
            query.push((wire, value.into()));
        }
    }
    if let Some(device) = device.as_deref() {
        query.push(("device_id", device.into()));
        query.push(("fp", format!("verify_{device}")));
    } else if let Some(fingerprint) = credentials["fingerprint"]
        .as_str()
        .filter(|value| !value.is_empty())
    {
        query.push(("fp", fingerprint.into()));
    }
    let query = query
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    Ok(format!("{BASE_URL}{path}?{query}"))
}

pub fn request(
    client: &reqwest::Client,
    credentials: &Value,
    path: &str,
) -> Result<reqwest::RequestBuilder, GatewayError> {
    let mut builder = client.post(endpoint(credentials, path)?);
    for (key, value) in headers(credentials)? {
        builder = builder.header(key, value);
    }
    Ok(builder)
}

/// 上游文案的统一前缀。
const LABEL: &str = "豆包工作：";

/// 给上游文案加一次「豆包工作：」前缀（**幂等**）。
///
/// ── 为什么必须幂等 ──────────────────────────────────────────
/// 转发失败时插件先把错误体（[`error_body`]）推给宿主，宿主紧接着又拿
/// **同一个体**回头调 `classify`；`classify` 会再走一次 [`error`]。不幂等的话
/// 用户看到的是「豆包工作：豆包工作：rate limited」—— 同一件事说两遍，还把
/// 真正有用的那半句挤到了后面（2026-11-02 在真实账号上逐字抓到）。
///
/// 判据用「是否已带前缀」而不是「调用次数」：两侧都可能先跑（宿主读 head 的
/// 错误体 / 先到的正文），靠计数去重会在任何一条路径改动后失效。
fn label_once(message: &str) -> String {
    if message.starts_with(LABEL) {
        message.to_string()
    } else {
        format!("{LABEL}{message}")
    }
}

/// 上游风控的人机验证判据：命中时返回场景名（如 `doubao_message_web`）。
///
/// ── 为什么必须单独认这一档 ──────────────────────────────────
/// 豆包风控（`shark_admin`）把「要人工过滑块」伪装成
/// `error_code=710022004 / error_msg=rate limited`：照字面读就是一条账号限额，
/// 于是用户看到的文案既说不清原因、也给不出下一步。实测（2026-11-02，两个
/// 账号复现）：去掉 `pc_version`、换参数组合、用浏览器 TLS 指纹重发，全都被
/// 同样拦下 —— 它是**账号 / 出口 IP 级的风控**，重发不会通过。
///
/// 真类型只写在 `extra.decision` 里（一段 JSON **字符串**）：
/// `{"from":"shark_admin","type":"verify","subtype":"slide","verify_scene":"…"}`。
/// 只认 `type=verify`，解析不出来就当普通错误，不去猜。
fn risk_verify(value: &Value) -> Option<String> {
    /// `decision` 里还塞着一段加密的挑战载荷，给个上限免得为一条错误体做大解析。
    const MAX_DECISION: usize = 64 * 1024;
    let extra = value
        .get("extra")
        .filter(|value| value.is_object())
        .unwrap_or(value);
    let decision = extra.get("decision").or_else(|| value.get("decision"))?;
    let parsed = match decision {
        Value::String(text) if text.len() <= MAX_DECISION => {
            serde_json::from_str::<Value>(text).ok()?
        }
        Value::Object(_) => decision.clone(),
        _ => return None,
    };
    if parsed.get("type").and_then(Value::as_str) != Some("verify") {
        return None;
    }
    Some(
        parsed
            .get("verify_scene")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    )
}

/// 风控要求人工验证时的文案：说清「这不是额度问题」与「下一步做什么」。
///
/// 状态码仍取 **429** —— 编排层对此的处置正是我们要的（把这个账号从本模型上
/// 挪开、换下一个候选），而 `mod.rs` 的 `classify` 按状态码分档，改动它的语义
/// 会牵动「退避 / 熔断」那一套（那是给瞬时故障准备的，对一个需要人工介入的
/// 账号更不合适）。
///
/// 文案里点明「所有模型都会被拦」：实测症状正是「请求任何模型都是同一条
/// 429」，不写清的话用户会逐个模型试过去（2026-11-02 的原始报告就是这样）。
fn verify_message(scene: &str) -> String {
    let scene = scene.trim();
    let scene = if scene.is_empty() {
        String::new()
    } else {
        format!("（场景 {scene}）")
    };
    format!(
        "触发了豆包上游的人机验证{scene}，不是账号额度问题（该账号当前所有模型都会被拦下）：\
         请在该账号的豆包网页端发一条消息、完成滑块验证后重试；\
         也可以等待约 30 分钟或更换出口网络再试。"
    )
}

pub fn error(status: u16, value: &Value) -> GatewayError {
    let detail = value
        .get("error_detail")
        .filter(|value| value.is_object())
        .or_else(|| value.get("error").filter(|value| value.is_object()))
        .unwrap_or(value);
    let code = number(&detail["code"])
        .or_else(|| number(&detail["error_code"]))
        .or_else(|| number(&value["code"]))
        .or_else(|| number(&value["error_code"]));
    let message = ["message", "msg", "error_msg", "error_message", "detail"]
        .iter()
        .find_map(|key| detail[*key].as_str().filter(|text| !text.trim().is_empty()))
        .or_else(|| value.as_str())
        .unwrap_or("上游返回了无法解析的错误");
    // 风控挑战先于状态码映射：`710022004` 只是它的外壳码，将来上游换码时
    // 「是不是人机验证」仍由 `extra.decision` 说了算。
    if let Some(scene) = risk_verify(value) {
        crate::logging::verbose(
            "[DoubaoWork]",
            &format!("上游要求人机验证（场景 {scene}），原始文案：{message}"),
        );
        return GatewayError::with_status(429, label_once(&verify_message(&scene)))
            .with_optional_code(code.and_then(|code| i64::try_from(code).ok()));
    }
    let lower = message.to_ascii_lowercase();
    let status = if matches!(code, Some(710012001))
        || status == 401
        || lower.contains("not logged in")
        || lower.contains("login required")
    {
        401
    } else if status == 429 || matches!(code, Some(710022004)) {
        429
    } else if status >= 400 {
        status
    } else {
        502
    };
    GatewayError::with_status(status as i32, label_once(message))
        .with_optional_code(code.and_then(|code| i64::try_from(code).ok()))
}

pub fn error_body(error: &GatewayError) -> Value {
    json!({"message":error.message,"code":error.upstream_code,"error":{"message":error.message,"code":error.upstream_code}})
}

pub async fn json_response(
    builder: reqwest::RequestBuilder,
    ug: bool,
) -> Result<Value, GatewayError> {
    let mut response = builder
        .send()
        .await
        .map_err(|error| GatewayError::with_status(502, crate::http::describe(&error)))?;
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| GatewayError::with_status(502, crate::http::describe(&error)))?
    {
        if bytes.len().saturating_add(chunk.len()) > 8 * 1024 * 1024 {
            return Err(GatewayError::with_status(502, "豆包工作控制面响应过大"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        GatewayError::with_status(502, "豆包工作未返回有效 JSON，请检查登录态或稍后重试")
    })?;
    if !(200..300).contains(&status)
        || number(&value["code"]).is_some_and(|code| code != 0 && !(ug && code == 200))
    {
        return Err(error(status, &value));
    }
    Ok(value)
}

pub async fn post(
    client: &reqwest::Client,
    credentials: &Value,
    path: &str,
    body: &Value,
) -> Result<Value, GatewayError> {
    json_response(
        request(client, credentials, path)?.json(body),
        path.starts_with("/alice/ug/"),
    )
    .await
}

pub async fn account(
    ctx: &PluginCtx,
    id: &str,
    stream: bool,
) -> Result<(String, Value, reqwest::Client), GatewayError> {
    let found = ctx.store_get(PROVIDER_ID, id).await.map_err(|error| {
        GatewayError::with_status(401, format!("读取豆包工作账号失败：{error}"))
    })?;
    let proxy = match found.proxy {
        Some(StoreProxy::Resolved { proxy }) => Some(proxy),
        Some(StoreProxy::Static { config }) => {
            match crate::proxy_util::resolve_account_proxy(Some(&config)) {
                Some(crate::proxy_util::ProxyResolution::Resolved(proxy)) => Some(proxy),
                Some(crate::proxy_util::ProxyResolution::Failed(message)) => {
                    return Err(GatewayError::with_status(502, message))
                }
                None => None,
            }
        }
        Some(StoreProxy::Failed { message }) => {
            return Err(GatewayError::with_status(502, message))
        }
        _ => None,
    };
    let credentials = super::accounts::resolve(&found.secret).await?;
    let client = client(proxy.as_ref(), &ctx.env.proxy, stream)?;
    if credentials["expectedUserId"].is_string() {
        let profile = super::accounts::identify(&client, &credentials).await?;
        if profile["userId"] != credentials["expectedUserId"]
            || profile["enterpriseType"] != credentials["expectedEnterpriseType"]
        {
            return Err(GatewayError::with_status(
                401,
                "豆包工作客户端已切换账号，请重新导入当前登录态",
            ));
        }
        // 旧版「只存身份」的记录：这次既然已经从客户端读到了登录态，顺手存一份
        // 快照（见 `accounts::upgrade_snapshot`），下次客户端不在也能用。
        // 失败不影响本次请求 —— 升级只是顺手，不是前置条件。
        if let Some(upgraded) = super::accounts::upgrade_snapshot(&found.secret, &credentials) {
            if let Err(error) = ctx.store_update(&found.id, &found.secret, &upgraded).await {
                crate::logging::verbose(
                    "[DoubaoWork]",
                    &format!("旧账号升级为登录态快照失败（本次仍可用）：{error}"),
                );
            }
        }
    }
    Ok((found.id, credentials, client))
}

pub fn classify(status: u16, body: &Value) -> proto::ErrorClass {
    let error = error(status, body);
    if error.status_code == 401 {
        return proto::ErrorClass::TokenExpired {
            message: error.message,
        };
    }
    if error.status_code == 429 {
        return proto::ErrorClass::QuotaLimited {
            reset_at: None,
            message: error.message,
            upstream_code: error.upstream_code,
            status: 429,
        };
    }
    proto::ErrorClass::Fatal {
        status: error.status_code as u16,
        message: error.message,
        upstream_code: error.upstream_code,
    }
}
