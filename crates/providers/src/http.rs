//! 出网：一个按「账号级代理 → 网关默认出口 → 直连」优先级建客户端的薄封装。
//!
//! ── 为什么各家都该用它，而不是各自 `reqwest::Client::new()` ──────
//! 三个理由，每一条都是踩过的坑：
//!   1. **代理**：账号可以配自己的出口（`ChatRequest.proxy`），没配时跟随网关
//!      的 `HostEnv.proxy`。自己 `new()` 的客户端会直连 —— 用户的代理配置
//!      静默失效，表现为「设了代理但还是超时」。
//!   2. **超时**：控制面请求该有 20 秒上限。用默认超时（无）会让一个挂住的
//!      上游把请求永远拖在那里。
//!   3. **连接复用**：每家一个 `Client` 就够（内部有连接池）。每次请求新建
//!      客户端会让每条请求多一次 TLS 握手。
//!
//! ── 为什么按代理 URL 缓存客户端 ──────────────────────────────
//! `reqwest::Client` 的连接池是**按代理**分的（一个客户端一个代理配置）。
//! 每次请求都 `Client::builder()...build()` 会丢掉复用，但按代理字符串缓存
//! 又能拿到复用 —— 账号级代理通常就那么几条，缓存不会膨胀。
//!
//! ── 网关默认出口为什么不存全局 ──────────────────────────────
//! `HostEnv` 在 `PluginCtx` 上（每次调用都能拿到），存一份进程级副本会多出
//! 「谁先 init、谁后 init」的时序问题。传参是零成本的，也更难写错。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use agent2api_provider_protocol as proto;
use serde_json::Value;

/// 控制面请求的超时（目录 / 余额 / 签到这类小请求）。
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(20);

// 计费接口在缺少 User-Agent 时会返回 403 / code=10085；请求级头可覆盖此默认值。
const DEFAULT_USER_AGENT: &str = "undici";

type ClientKey = (String, Option<crate::timeouts::Timeouts>);

fn cache() -> &'static Mutex<HashMap<ClientKey, reqwest::Client>> {
    static CACHE: OnceLock<Mutex<HashMap<ClientKey, reqwest::Client>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取控制面客户端，带 20 秒总超时，不能用于读取对话流。
///
/// 账号级代理优先，其次 `gateway_proxy`（空串或 `direct` 表示直连）。
/// 代理 URL 非法时降级直连。
pub fn control_client(proxy: Option<&proto::WireProxy>, gateway_proxy: &str) -> reqwest::Client {
    cached_client(proxy, gateway_proxy, None)
}

/// 长响应只限制建连与读取空闲，不设置整个响应体的总时限。
pub fn stream_client(proxy: Option<&proto::WireProxy>, gateway_proxy: &str) -> reqwest::Client {
    cached_client(proxy, gateway_proxy, Some(crate::timeouts::current()))
}

fn cached_client(
    proxy: Option<&proto::WireProxy>,
    gateway_proxy: &str,
    timeouts: Option<crate::timeouts::Timeouts>,
) -> reqwest::Client {
    let proxy_url = match proxy {
        Some(proxy) => proxy.to_url(),
        None => gateway_proxy.trim().to_string(),
    };
    let key = (proxy_url, timeouts);
    if let Ok(guard) = cache().lock() {
        if let Some(client) = guard.get(&key) {
            return client.clone();
        }
    }
    let built = build(&key.0, timeouts);
    if let Ok(mut guard) = cache().lock() {
        if guard.len() >= 128 { guard.clear(); }
        guard.insert(key, built.clone());
    }
    built
}

/// 建一个客户端（代理 URL 为空 / `direct` = 直连）。
fn build(proxy_url: &str, timeouts: Option<crate::timeouts::Timeouts>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        // 与网关同一条栈：rustls，不依赖系统 openssl
        .use_rustls_tls()
        .user_agent(DEFAULT_USER_AGENT);
    builder = match timeouts {
        Some(timeouts) => builder
            .connect_timeout(Duration::from_millis(timeouts.connect_ms()))
            .read_timeout(Duration::from_millis(timeouts.read_timeout_backstop_ms())),
        None => builder.timeout(CONTROL_TIMEOUT),
    };
    let trimmed = proxy_url.trim();
    if trimmed.is_empty() || trimmed == "direct" {
        // ── `no_proxy()` 为什么必须显式调 ──────────────────────────
        // `reqwest` 开了 `system-proxy` 特性（Antigravity 要跟随系统代理，
        // 见 [`control_client_system_proxy`]）之后，**默认**就会读系统设置。而仓内
        // 其余二十家的口径是「直连就是直连」（出口由账号代理决定，不被机器
        // 环境悄悄改写）—— 少了这一句，那些家的直连请求会突然走用户系统里
        // 配的代理，症状是「设了账号代理反而更慢 / 没设代理却报代理错误」。
        builder = builder.no_proxy();
    } else {
        match reqwest::Proxy::all(trimmed) {
            Ok(proxy) => builder = builder.proxy(proxy),
            Err(_) => {
                // 见 `control_client` 的说明：降级直连，不阻断链路
                builder = builder.no_proxy();
            }
        }
    }
    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

/// 跟随**系统代理**的客户端（进程级一份）。
///
/// ── 只有一家该用它 ──────────────────────────────────────────
/// Antigravity 的上游是 Google：多数网络里只有经代理才可达，而用户机器上
/// 「已经能打开 Google 的那个代理」就写在系统设置里。账号没单独配代理时
/// 跟随它是唯一合理的默认（`Provider::meta` 的
/// `capabilities.system_proxy_when_unset = true` 就是这个意思）。
///
/// **别把这条抄到别家** —— 对国内可直连的上游，「直连就是直连」才是正确
/// 口径（见 [`build`] 里 `no_proxy()` 的说明）。
///
/// 与 [`control_client`] 不同**不进缓存表**：它没有「出口参数」这一维，进程内一份
/// 即可。系统代理改了要重启插件才生效（reqwest 在 build 时探测一次）——
/// 登录与 Antigravity 的请求都是低频操作，这个取舍不影响正确性。
pub fn control_client_system_proxy() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            // 既不调 `.no_proxy()` 也不调 `.proxy()`：reqwest 的 auto_sys_proxy
            // 因此保持开启（`system-proxy` 特性，见 Cargo.toml 的说明）
            reqwest::Client::builder()
                .use_rustls_tls()
                .timeout(CONTROL_TIMEOUT)
                .user_agent(DEFAULT_USER_AGENT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}

/// 发一次 GET 并解析 JSON。
///
/// 返回 `(status, payload)`：**非 2xx 也返回**（不在这里判成败）——
/// 各家对「哪些状态码算失败」的判据不同（魔搭用 200 + 业务位表达失败，
/// Command Code 用 401/403），把判定权留给调用方。
pub async fn get_json(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
) -> Result<(u16, Value), String> {
    let mut request = client.get(url).header("Accept", "application/json");
    for (key, value) in headers {
        request = request.header(key, value);
    }
    let response = request.send().await.map_err(|error| describe(&error))?;
    let status = response.status().as_u16();
    let text = response.text().await.map_err(|error| describe(&error))?;
    Ok((status, parse_json(&text)))
}

/// 发一次 POST（JSON 体）并解析 JSON 响应。
pub async fn post_json(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
) -> Result<(u16, Value), String> {
    let mut request = client
        .post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .body(body.to_string());
    for (key, value) in headers {
        request = request.header(key, value);
    }
    let response = request.send().await.map_err(|error| describe(&error))?;
    let status = response.status().as_u16();
    let text = response.text().await.map_err(|error| describe(&error))?;
    Ok((status, parse_json(&text)))
}

/// 发一次 POST（JSON 体），返回**未读的响应**（调用方自己消费流）。
///
/// ── 什么时候用这个而不是 [`post_json`] ──────────────────────
/// 上游回 SSE 时（`conversation` 的常见形态）：`post_json` 会把整个响应体
/// 读进内存再解析 JSON，对一条流式回答来说那是「等上游说完才返回」——
/// 客户端看到的是几分钟的空白。
///
/// 非 2xx 也返回结果（与 [`get_json`] 同口径：判定权在调用方）；但错误体
/// 会被读出来放进 [`SseResponse::error_body`]，免得调用方忘了读。
pub async fn post_stream(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
    accept: &str,
) -> Result<SseResponse, String> {
    let mut request = client
        .post(url)
        .header("Accept", accept)
        .header("Content-Type", "application/json")
        .body(body.to_string());
    for (key, value) in headers {
        request = request.header(key, value);
    }
    let response = request.send().await.map_err(|error| describe(&error))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let text = response.text().await.unwrap_or_default();
        return Ok(SseResponse { status, response: None, error_body: Some(text) });
    }
    Ok(SseResponse { status, response: Some(response), error_body: None })
}

/// [`post_stream`] 的结果。
pub struct SseResponse {
    /// 上游状态码。
    pub status: u16,
    /// 2xx 时的响应（未读的流）。
    pub response: Option<reqwest::Response>,
    /// 非 2xx 时的错误体（已读完）。
    pub error_body: Option<String>,
}

impl SseResponse {
    /// 是否成功。
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// 错误体的前 240 个字符（拼错误文案用）。
    pub fn error_head(&self) -> String {
        self.error_body.as_deref().unwrap_or("").chars().take(240).collect()
    }
}

/// 一次请求的原始结果（网关 `auth_http::ApiResponse` 的插件侧等价物）。
///
/// ── 为什么非 2xx 也返回 `Ok` ────────────────────────────────
/// 与 [`get_json`] 同一口径：各家对「哪些状态码算失败」的判据不同
/// （LobsterAI 的 402 是余额耗尽、AtomCode 的 403 可能是缺签名）。
/// 判定权留给调用方，这里只如实转述。
pub struct ApiResponse {
    /// HTTP 状态码。
    pub status: u16,
    /// 是否 2xx。
    pub ok: bool,
    /// 解析出的 JSON（空体 / 非 JSON 一律 `None`）。
    pub payload: Option<Value>,
}

/// 发一次请求并返回原始结果（网关 `auth_http::send_raw` 的插件侧等价物）。
///
/// ── 与 [`get_json`] / [`post_json`] 的分工 ──────────────────
/// 那两个是「便捷形态」（固定方法、无超时参数、非 2xx 也给 `Ok`）。
/// 搬来的各家 `credentials.rs` / `usage.rs` 用的是 `send_raw(method, url, body,
/// headers, proxy, timeout)` 那个通用形态 —— 它们要显式指定超时（鉴权接口
/// 30 秒、目录 20 秒、账单 25 秒各不相同），也要在传输失败时拿到
/// `reqwest::Error` 好判 `is_timeout()`。所以这里原样复刻那个签名。
///
/// `proxy` 为 `None` 时**直连**（与网关 `send_raw` 的语义逐字一致：那个函数
/// 收 `Option<&ResolvedProxy>`，`None` 走 `client_for(None)` = 直连，**不是**
/// 跟随网关默认出口）。搬来的「鉴权接口不走账号代理」那几处正是这个语义。
pub async fn send_raw(
    method: &str,
    url: &str,
    body: Option<&Value>,
    headers: &[(String, String)],
    proxy: Option<&proto::WireProxy>,
    timeout_ms: Option<u64>,
) -> Result<ApiResponse, reqwest::Error> {
    let client = match proxy {
        Some(proxy) => control_client(Some(proxy), ""),
        // `direct` 是显式直连，不跟随系统代理。
        None => control_client(None, "direct"),
    };
    let mut builder = match method {
        "POST" => client.post(url),
        "PUT" => client.put(url),
        "DELETE" => client.delete(url),
        "PATCH" => client.patch(url),
        _ => client.get(url),
    };
    builder = builder.header("Accept", "application/json");
    for (key, value) in headers {
        builder = builder.header(key.as_str(), value.as_str());
    }
    if let Some(payload) = body {
        builder = builder
            .header("Content-Type", "application/json")
            .body(payload.to_string());
    }
    if let Some(timeout) = timeout_ms {
        builder = builder.timeout(Duration::from_millis(timeout));
    }
    let response = builder.send().await?;
    let status = response.status().as_u16();
    let ok = response.status().is_success();
    let text = response.text().await?;
    let payload = if text.trim().is_empty() {
        None
    } else {
        serde_json::from_str::<Value>(&text).ok()
    };
    Ok(ApiResponse { status, ok, payload })
}

/// 空体 / 非 JSON 一律给 `Value::Null`（与网关 `send_raw` 同口径）。
fn parse_json(text: &str) -> Value {
    if text.trim().is_empty() {
        return Value::Null;
    }
    serde_json::from_str(text).unwrap_or(Value::Null)
}

/// 传输错误 → 人话（保留原因，便于排障）。
pub fn describe(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "连接上游超时".to_string()
    } else if error.is_connect() {
        format!("无法连接上游: {error}")
    } else {
        format!("请求上游失败: {error}")
    }
}

/// 传输错误 → **完整原因链**（与网关 `egress::describe_error_detail` 同义）。
///
/// ── 为什么不能只用顶层文案 ──────────────────────────────────
/// `reqwest::Error` 的 `Display` 只给最外层一句话（`error sending request`），
/// 真正的原因（DNS 解析失败 / TLS 握手 / 连接被重置）在 `source()` 链里。
/// 排障时缺了那一段就只能看到「请求上游失败」，而看不出是哪一步。
///
/// 从网关搬来的各家（chatglm / autoclaw / …）用它拼错误文案，所以这里补一份
/// 同口径实现，让那些文案逐字不变。
pub fn describe_chain(error: &reqwest::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    let head = error.to_string();
    if !head.is_empty() {
        parts.push(head);
    }
    let mut current: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(error);
    while let Some(source) = current {
        let text = source.to_string();
        if !text.is_empty() && !parts.iter().any(|part| part == &text) {
            parts.push(text);
        }
        current = source.source();
    }
    if parts.is_empty() {
        return "未知错误".to_string();
    }
    parts.join(" → ")
}

/// 当前毫秒时间戳。
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}
