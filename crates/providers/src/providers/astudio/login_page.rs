//! AStudio 的本机登录页：手机号验证码 / 账号密码（含极验滑块）。
//!
//! ── 为什么需要一个页面 ──────────────────────────────────────
//! 发短信验证码与账号密码登录都要极验的 `geetest_challenge/validate/seccode`，
//! 而极验必须在浏览器里完成。插件起一个只监听 `127.0.0.1` 的小服务，页面
//! 里加载极验脚本、完成滑块后把结果 POST 回来；插件再把结果转成上游的
//! multipart 请求（见 `auth.rs`）。
//!
//! ── 安全 ────────────────────────────────────────────────────
//! 只绑回环地址；每个流程有一个随机 token（URL 的 `?t=`，API 也校验），
//! 防止本机其它进程误打。密码只在内存里转发，不落盘、不写日志。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use agent2api_provider_protocol as proto;

use super::{auth, login};

/// 内嵌的登录页（HTML + 极验 v3 初始化 + 表单逻辑）。
const PAGE: &str = include_str!("login_page.html");

/// 流程最长等待时间。
const LOGIN_TIMEOUT_MS: u64 = 10 * 60 * 1000;

struct WebState {
    flow: String,
    token: String,
    proxy: Option<proto::WireProxy>,
    gateway_proxy: String,
}

/// 起一个本机登录页，返回给网关的登录地址。
pub async fn start(
    proxy: Option<proto::WireProxy>,
    gateway_proxy: String,
) -> Result<proto::LoginStart, String> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|error| format!("本机登录页启动失败：{error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("本机登录页启动失败：{error}"))?
        .port();
    let flow = crate::ids::random_id("astudio-web");
    login::open_flow(&flow);
    let token = random_token();
    let state = Arc::new(WebState {
        flow: flow.clone(),
        token: token.clone(),
        proxy,
        gateway_proxy,
    });
    let handle = tokio::spawn(serve(listener, state));
    login::attach_abort(&flow, handle.abort_handle());
    let timeout_flow = flow.clone();
    let timeout_handle = handle.abort_handle();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(LOGIN_TIMEOUT_MS)).await;
        login::store_result(&timeout_flow, Err("登录超时，请重新发起".into()));
        timeout_handle.abort();
    });
    Ok(proto::LoginStart {
        flow,
        url: format!("http://127.0.0.1:{port}/?t={token}"),
        instructions: String::new(),
        poll_interval_ms: 1500,
        // 不强制系统浏览器：用户选「内嵌窗口」时就开内嵌窗口。
        external_browser: false,
        // 网关只放行「HTTPS 站点」与「授权地址里登记的回环回调」；本机登录页
        // 既不是 HTTPS 也不是回调，所以按「带端口的回环主机」明确申报。
        allowed_hosts: vec![format!("127.0.0.1:{port}")],
    })
}

async fn serve(listener: tokio::net::TcpListener, state: Arc<WebState>) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            break;
        };
        let state = state.clone();
        tokio::spawn(async move {
            let request = read_request(&mut stream).await;
            let response = match request {
                Some(request) => route(&state, request).await,
                None => error_response(400, "请求格式不正确"),
            };
            let _ = write_response(&mut stream, &response).await;
        });
    }
}

struct Request {
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<Request> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            break position;
        }
        if buffer.len() > 64 * 1024 {
            return None;
        }
        match stream.read(&mut chunk).await {
            Ok(0) => return None,
            Ok(count) => buffer.extend_from_slice(&chunk[..count]),
            Err(_) => return None,
        }
    };
    let header_text = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = header_text.lines();
    let first = lines.next()?;
    let mut parts = first.split_whitespace();
    let _method = parts.next()?;
    let target = parts.next()?;
    let mut headers = HashMap::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0)
        .min(256 * 1024);
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < length {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(count) => body.extend_from_slice(&chunk[..count]),
            Err(_) => break,
        }
    }
    body.truncate(length);
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), parse_query(query)),
        None => (target.to_string(), HashMap::new()),
    };
    Some(Request {
        path,
        query,
        headers,
        body,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        out.insert(percent_decode(key), percent_decode(value));
    }
    out
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

fn json_response(status: u16, value: &Value) -> Response {
    Response {
        status,
        content_type: "application/json; charset=utf-8",
        body: serde_json::to_vec(value).unwrap_or_default(),
    }
}

fn error_response(status: u16, message: &str) -> Response {
    json_response(status, &json!({ "error": message }))
}

async fn write_response(
    stream: &mut tokio::net::TcpStream,
    response: &Response,
) -> std::io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        502 => "Bad Gateway",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        reason,
        response.content_type,
        response.body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&response.body).await?;
    stream.flush().await
}

async fn route(state: &WebState, request: Request) -> Response {
    let provided = request
        .query
        .get("t")
        .cloned()
        .or_else(|| request.headers.get("x-login-token").cloned())
        .unwrap_or_default();
    if provided != state.token {
        return error_response(403, "登录页凭证不正确，请重新发起登录");
    }
    match request.path.as_str() {
        "/" | "/index.html" => Response {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: PAGE.as_bytes().to_vec(),
        },
        "/api/geetest" => {
            match auth::geetest_config(state.proxy.as_ref(), &state.gateway_proxy).await {
                Ok(config) => json_response(200, &config),
                Err(message) => error_response(502, &message),
            }
        }
        "/api/send-code" => {
            let payload = match parse_body(&request.body) {
                Some(payload) => payload,
                None => return error_response(400, "请求内容不是 JSON"),
            };
            let mobile = text_of(&payload, "mobile");
            if !auth::valid_mobile(&mobile) {
                return error_response(400, "请输入 11 位手机号");
            }
            let Some(captcha) = captcha_of(&payload) else {
                return error_response(400, "请先完成滑块验证");
            };
            match auth::send_sms(
                state.proxy.as_ref(),
                &state.gateway_proxy,
                &mobile,
                &captcha,
            )
            .await
            {
                Ok(()) => json_response(200, &json!({ "ok": true })),
                Err(message) => error_response(502, &message),
            }
        }
        "/api/login-sms" => {
            let payload = match parse_body(&request.body) {
                Some(payload) => payload,
                None => return error_response(400, "请求内容不是 JSON"),
            };
            let mobile = text_of(&payload, "mobile");
            let code = text_of(&payload, "verifyCode");
            if !auth::valid_mobile(&mobile) {
                return error_response(400, "请输入 11 位手机号");
            }
            if !auth::valid_code(&code) {
                return error_response(400, "请输入 6 位验证码");
            }
            match auth::login_sms(state.proxy.as_ref(), &state.gateway_proxy, &mobile, &code).await
            {
                Ok(session) => finish(state, &session, "sms"),
                Err(message) => error_response(502, &message),
            }
        }
        "/api/login-password" => {
            let payload = match parse_body(&request.body) {
                Some(payload) => payload,
                None => return error_response(400, "请求内容不是 JSON"),
            };
            let account = text_of(&payload, "accountName");
            // 密码按原样提交（前后空格可能是有意义的字符）。
            let password = payload
                .get("accountPwd")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if account.is_empty() {
                return error_response(400, "请输入账号");
            }
            if password.len() < 6 || password.len() > 20 {
                return error_response(400, "密码长度必须为 6–20 位");
            }
            let Some(captcha) = captcha_of(&payload) else {
                return error_response(400, "请先完成滑块验证");
            };
            match auth::login_password(
                state.proxy.as_ref(),
                &state.gateway_proxy,
                &account,
                &password,
                &captcha,
            )
            .await
            {
                Ok(session) => finish(state, &session, "password"),
                Err(message) => error_response(502, &message),
            }
        }
        _ => error_response(404, "没有这个接口"),
    }
}

fn finish(state: &WebState, session: &auth::Session, method: &str) -> Response {
    let secret = secret_of(session, method);
    login::store_result(&state.flow, Ok(secret));
    json_response(200, &json!({ "ok": true }))
}

fn parse_body(body: &[u8]) -> Option<Value> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .filter(Value::is_object)
}

fn text_of(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn captcha_of(value: &Value) -> Option<auth::Captcha> {
    let object = value.get("captcha")?;
    let pick = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let captcha = auth::Captcha {
        challenge: pick("geetest_challenge"),
        validate: pick("geetest_validate"),
        seccode: pick("geetest_seccode"),
    };
    captcha.is_complete().then_some(captcha)
}

/// 登录结果 → 账号 secret（与桌面端读取产出的结构一致）。
fn secret_of(session: &auth::Session, method: &str) -> Value {
    let name = if !session.mobile.is_empty() {
        session.mobile.clone()
    } else if !session.nickname.is_empty() {
        session.nickname.clone()
    } else if !session.uid.is_empty() {
        session.uid.clone()
    } else {
        "AStudio".to_string()
    };
    let mut secret = json!({
        "name": name,
        "key": session.model_bearer_token,
        "uid": session.uid,
        "source": method,
        "session": {
            "token": session.token,
            "accountId": session.account_id,
        }
    });
    if !session.sso_session_id.is_empty() {
        secret["session"]["ssoSessionId"] = json!(session.sso_session_id);
    }
    secret
}

fn random_token() -> String {
    let mut bytes = [0u8; 24];
    if getrandom::getrandom(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        bytes[..16].copy_from_slice(&nanos.to_le_bytes());
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
