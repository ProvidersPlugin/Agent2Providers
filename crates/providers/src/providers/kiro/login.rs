//! Kiro 浏览器登录：Kiro 门户的 OAuth（Google / GitHub / AWS Builder ID /
//! IAM Identity Center），回调落在本机 loopback 端口。
//!
//! ── 流程 ────────────────────────────────────────────────────
//! ```text
//! ① 生成 state + PKCE verifier，监听 Kiro IDE 用的那组端口之一
//! ② 打开 https://app.kiro.dev/signin?…
//! ③ 用户在门户里选登录方式：
//!      Google / GitHub  → 带回 code，POST prod.us-east-1.auth.desktop.kiro.dev/oauth/token
//!      Builder ID / IDC → 带回 AWS 登录地址与地区 → 注册 AWS OIDC 客户端 → 302 到 AWS
//!      AWS 回调         → POST oidc.{region}.amazonaws.com/token
//! ④ 拿到 token 后问一次 Get-Usage-Limits 取邮箱与套餐，落成账号
//! ```
//!
//! ── 为什么报 `poll_interval_ms > 0` ─────────────────────────
//! 回调打在**插件自己**的 loopback 端口上，永远不经过网关的
//! `/api/session/login/callback`。报 0 的话网关会一直等一个不会到来的回调。
//!
//! ── 远程部署 ────────────────────────────────────────────────
//! 参考实现支持把浏览器最终的 `http://127.0.0.1:<port>/…` 地址粘贴回来。
//! 本版本先只做本机自动完成（网关与本机浏览器同机）；远程场景请用
//! 「导入 kiro-cli / Kiro IDE 登录态」或 API key。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use agent2api_provider_protocol as proto;

use super::credentials::{identity, now_ms, public, Credentials};
use super::{endpoints, usage};
use crate::logging;

/// 一轮登录在进程内的状态。
#[derive(Default)]
struct Flow {
    proxy: Option<proto::WireProxy>,
    state: String,
    verifier: String,
    redirect: String,
    stage: String,
    aws: Option<AwsPending>,
    result: Option<Result<Value, String>>,
    cancelled: bool,
    abort: Option<tokio::task::AbortHandle>,
}

struct AwsPending {
    region: String,
    client_id: String,
    client_secret: String,
    provider: String,
    state: String,
    verifier: String,
    on_to_aws: String,
    portal_asked: String,
}

fn flows() -> &'static Mutex<HashMap<String, Arc<Mutex<Flow>>>> {
    static FLOWS: OnceLock<Mutex<HashMap<String, Arc<Mutex<Flow>>>>> = OnceLock::new();
    FLOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_table<T>(action: impl FnOnce(&mut HashMap<String, Arc<Mutex<Flow>>>) -> T) -> T {
    match flows().lock() {
        Ok(mut table) => action(&mut table),
        Err(poisoned) => action(&mut poisoned.into_inner()),
    }
}

fn with_flow<T>(flow: &Arc<Mutex<Flow>>, action: impl FnOnce(&mut Flow) -> T) -> T {
    match flow.lock() {
        Ok(mut state) => action(&mut state),
        Err(poisoned) => action(&mut poisoned.into_inner()),
    }
}

/// 随机串（base64url，无填充）。
fn random_token(bytes: usize) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let mut buffer = vec![0u8; bytes];
    if getrandom::getrandom(&mut buffer).is_err() {
        let seed = now_ms().to_string();
        buffer = seed.into_bytes();
        buffer.resize(bytes, b'0');
    }
    URL_SAFE_NO_PAD.encode(buffer)
}

fn pkce_challenge(verifier: &str) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use sha2::{Digest, Sha256};
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// 发起一次登录：选端口、起回调服务、返回授权地址。
pub async fn start(proxy: Option<&proto::WireProxy>) -> Result<proto::LoginStart, String> {
    let flow_id = format!("kiro-{}", random_token(9));
    let state = random_token(24);
    let verifier = random_token(48);
    let (listener, port) = listen().await?;
    let redirect = format!("http://localhost:{port}");
    let aws_redirect = format!("http://127.0.0.1:{port}/oauth/callback");
    let flow = Arc::new(Mutex::new(Flow {
        proxy: proxy.cloned(),
        state: state.clone(),
        verifier: verifier.clone(),
        redirect: redirect.clone(),
        stage: "portal".into(),
        ..Default::default()
    }));
    with_table(|table| table.insert(flow_id.clone(), flow.clone()));

    let server_flow = flow.clone();
    let server_aws_redirect = aws_redirect.clone();
    let handle = tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(item) => item,
                Err(_) => break,
            };
            let flow = server_flow.clone();
            let aws_redirect = server_aws_redirect.clone();
            tokio::spawn(async move {
                let request = read_request(&mut stream).await;
                let response = handle_request(&flow, &request, &aws_redirect).await;
                let _ = write_response(&mut stream, &response).await;
            });
        }
    });
    with_flow(&flow, |state| state.abort = Some(handle.abort_handle()));

    // 超时：到点还没完成就标记失败，并释放端口。
    let timeout_flow = flow.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(
            endpoints::SIGN_IN_TIMEOUT_MS,
        ))
        .await;
        with_flow(&timeout_flow, |state| {
            if state.result.is_none() {
                state.result = Some(Err("登录超时，请重新发起".into()));
            }
            if let Some(abort) = state.abort.take() {
                abort.abort();
            }
        });
    });

    let challenge = pkce_challenge(&verifier);
    let url = format!(
        "{}/signin?state={}&code_challenge={}&code_challenge_method=S256&redirect_uri={}&redirect_from=KiroIDE",
        endpoints::PORTAL,
        state,
        challenge,
        urlencode(&redirect)
    );
    Ok(proto::LoginStart {
        flow: flow_id,
        url,
        instructions: "用 Google、GitHub、AWS Builder ID 或 IAM Identity Center 登录 Kiro；完成后这个窗口会自动关闭。"
            .into(),
        poll_interval_ms: 1500,
        external_browser: true,
        allowed_hosts: vec![
            "app.kiro.dev".into(),
            "view.awsapps.com".into(),
            "amazonaws.com".into(),
            "signin.aws.amazon.com".into(),
        ],
    })
}

/// 轮询一次登录进度。
pub async fn poll(flow_id: &str) -> proto::LoginPoll {
    let flow = with_table(|table| table.get(flow_id).cloned());
    let Some(flow) = flow else {
        return proto::LoginPoll::Failed {
            message: "这次登录已失效，请重新发起".into(),
        };
    };
    let result = with_flow(&flow, |state| state.result.clone());
    match result {
        Some(Ok(secret)) => {
            with_table(|table| table.remove(flow_id));
            let view = public(&secret);
            proto::LoginPoll::Done {
                account: proto::NormalizedAccount {
                    identity: identity(&secret),
                    public: view,
                    secret,
                    desktop: false,
                },
            }
        }
        Some(Err(message)) => {
            with_table(|table| table.remove(flow_id));
            proto::LoginPoll::Failed { message }
        }
        None => proto::LoginPoll::Pending {
            hint: Some("等待浏览器里的登录完成…".into()),
        },
    }
}

/// 取消一轮登录。
pub fn cancel(flow_id: &str) {
    if let Some(flow) = with_table(|table| table.remove(flow_id)) {
        with_flow(&flow, |state| {
            state.cancelled = true;
            state.result = Some(Err("登录已取消".into()));
            if let Some(abort) = state.abort.take() {
                abort.abort();
            }
        });
    }
}

/// 立刻完成的一轮登录（本机登录态已经读到，不需要等浏览器）。
pub fn immediate(secret: Value) -> proto::LoginStart {
    let flow_id = format!("kiro-local-{}", random_token(9));
    let flow = Arc::new(Mutex::new(Flow {
        result: Some(Ok(secret)),
        stage: "over".into(),
        ..Default::default()
    }));
    with_table(|table| {
        table.insert(flow_id.clone(), flow);
    });
    proto::LoginStart {
        flow: flow_id,
        url: String::new(),
        instructions: "已读取本机 Kiro 登录态。".into(),
        poll_interval_ms: 1000,
        external_browser: false,
        allowed_hosts: Vec::new(),
    }
}

/// 使用本机 kiro-cli / Kiro IDE 的登录态（点一下就完成）。
pub async fn start_local() -> Result<proto::LoginStart, String> {
    let credentials = super::refresh::read_local("kiro").await.ok_or_else(|| {
        "没有找到 Kiro 的本机登录态：请先在 kiro-cli 或 Kiro IDE 里登录".to_string()
    })?;
    let name = if !credentials.account_id.is_empty() {
        credentials.account_id.clone()
    } else if credentials.source == "kiro-cli" {
        "kiro-cli 的账号".to_string()
    } else {
        "Kiro IDE 的账号".to_string()
    };
    let mut secret = json!({ "name": name });
    credentials.write_into(&mut secret);
    Ok(immediate(secret))
}

/// 逐个尝试 Kiro IDE 的回调端口。
async fn listen() -> Result<(tokio::net::TcpListener, u16), String> {
    for port in endpoints::CALLBACK_PORTS {
        if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", *port)).await {
            return Ok((listener, *port));
        }
    }
    Err("Kiro 登录要用的本机端口都被占用了，请关闭 Kiro IDE 的登录后重试".to_string())
}

/// 一次 HTTP 请求的解析结果。
struct Request {
    path: String,
    query: HashMap<String, String>,
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Request {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(count) => {
                buffer.extend_from_slice(&chunk[..count]);
                if buffer.windows(4).any(|window| window == b"\r\n\r\n") || buffer.len() > 64 * 1024
                {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&buffer);
    let first = text.lines().next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let _method = parts.next().unwrap_or("GET");
    let target = parts.next().unwrap_or("/");
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), parse_query(query)),
        None => (target.to_string(), HashMap::new()),
    };
    Request { path, query }
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
        // 只按**字节**判十六进制：直接切字符串会让未编码的多字节字符
        // 落在非字符边界上 panic（crate 里 panic 会带走整个插件进程）。
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        if bytes[index] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[index]);
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn urlencode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: String,
    location: Option<String>,
}

impl Response {
    fn html(status: u16, body: String) -> Self {
        Self {
            status,
            content_type: "text/html; charset=utf-8",
            body,
            location: None,
        }
    }
    fn redirect(location: String) -> Self {
        Self {
            status: 302,
            content_type: "text/html; charset=utf-8",
            body: String::new(),
            location: Some(location),
        }
    }
}

async fn write_response(
    stream: &mut tokio::net::TcpStream,
    response: &Response,
) -> std::io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        302 => "Found",
        404 => "Not Found",
        409 => "Conflict",
        _ => "OK",
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        reason,
        response.content_type,
        response.body.len()
    );
    if let Some(location) = &response.location {
        head.push_str(&format!("Location: {location}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(response.body.as_bytes()).await?;
    stream.flush().await
}

fn page(ok: bool, title: &str, text: &str) -> String {
    let color = if ok { "#2a9d5c" } else { "#c0392b" };
    format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{title}</title>\
         <style>body{{font:15px system-ui,sans-serif;display:grid;place-items:center;min-height:90vh;margin:0;color:#222;background:#fafafa}}\
         @media (prefers-color-scheme:dark){{body{{color:#eee;background:#161616}}}}\
         main{{text-align:center;max-width:28rem;padding:1rem}}.d{{font-size:2rem;color:{color}}}</style>\
         <main><div class=\"d\">{mark}</div><h1>{title}</h1><p>{text}</p></main>",
        mark = if ok { "✓" } else { "✕" },
    )
}

async fn handle_request(
    flow: &Arc<Mutex<Flow>>,
    request: &Request,
    aws_redirect: &str,
) -> Response {
    if request.path != "/oauth/callback" && request.path != "/signin/callback" {
        return Response::html(404, page(false, "Not found", ""));
    }
    let (stage, cancelled, flow_state, aws, proxy_url) = with_flow(flow, |state| {
        (
            state.stage.clone(),
            state.cancelled,
            state.state.clone(),
            state.aws.as_ref().map(|aws| AwsPending {
                region: aws.region.clone(),
                client_id: aws.client_id.clone(),
                client_secret: aws.client_secret.clone(),
                provider: aws.provider.clone(),
                state: aws.state.clone(),
                verifier: aws.verifier.clone(),
                on_to_aws: aws.on_to_aws.clone(),
                portal_asked: aws.portal_asked.clone(),
            }),
            state
                .proxy
                .as_ref()
                .map(|proxy| proxy.to_url())
                .unwrap_or_default(),
        )
    });
    if cancelled || stage == "over" {
        return Response::html(404, page(false, "这次登录已结束", "请重新发起登录。"));
    }
    let got = request.query.get("state").cloned().unwrap_or_default();
    let portal = got == flow_state;
    let from_aws = aws
        .as_ref()
        .is_some_and(|aws| !aws.state.is_empty() && got == aws.state);
    if !portal && !from_aws {
        return Response::html(
            404,
            page(
                false,
                "这个地址不属于本次登录",
                "请从刚打开的登录页重新操作，或重新发起登录。",
            ),
        );
    }
    if let Some(error) = request.query.get("error") {
        let detail = request
            .query
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| error.clone());
        return fail(flow, &detail);
    }

    if from_aws {
        let Some(aws) = aws else {
            return fail(flow, "AWS 回调缺少登录状态");
        };
        if request.path != "/oauth/callback" {
            return Response::html(404, page(false, "这个地址不属于本次登录", ""));
        }
        let Some(code) = request.query.get("code").cloned() else {
            return fail(flow, "AWS 没有带回授权码");
        };
        let body = json!({
            "clientId": aws.client_id,
            "clientSecret": aws.client_secret,
            "grantType": "authorization_code",
            "redirectUri": aws_redirect,
            "code": code,
            "codeVerifier": aws.verifier,
        });
        let url = format!("{}/token", endpoints::oidc(&aws.region));
        let token = match post_json(&proxy_url, &url, &body, &[]).await {
            Ok(token) => token,
            Err(error) => return fail(flow, &error),
        };
        let access = token
            .get("accessToken")
            .and_then(Value::as_str)
            .unwrap_or("");
        if access.is_empty() {
            return fail(flow, "AWS 没有返回 token");
        }
        let credentials = Credentials {
            access: access.to_string(),
            refresh: token
                .get("refreshToken")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            expires: expiry(&token, "expiresIn"),
            method: "idc".into(),
            login_provider: login_provider(&aws.provider),
            uid: login_provider(&aws.provider)
                .to_lowercase()
                .replace(' ', ""),
            region: aws.region.clone(),
            client_id: aws.client_id.clone(),
            client_secret: aws.client_secret.clone(),
            ..Default::default()
        };
        return finish(flow, credentials).await;
    }

    let option = request
        .query
        .get("login_option")
        .cloned()
        .unwrap_or_default();
    let asked = format!(
        "{}|{}|{}",
        option,
        request.query.get("issuer_url").cloned().unwrap_or_default(),
        request.query.get("idc_region").cloned().unwrap_or_default()
    );
    if stage == "aws" {
        if let Some(aws) = &aws {
            if !aws.on_to_aws.is_empty() && asked == aws.portal_asked {
                return Response::redirect(aws.on_to_aws.clone());
            }
        }
        with_flow(flow, |state| {
            state.stage = "portal".into();
            state.aws = None;
        });
    }
    match option.as_str() {
        "google" | "github" => {
            let Some(code) = request.query.get("code").cloned() else {
                return fail(flow, "Kiro 登录页没有带回授权码");
            };
            let verifier = with_flow(flow, |state| state.verifier.clone());
            let redirect = with_flow(flow, |state| state.redirect.clone());
            let redirect_uri = format!("{redirect}{}?login_option={option}", request.path);
            let body = json!({
                "code": code,
                "code_verifier": verifier,
                "redirect_uri": redirect_uri,
            });
            let url = format!("{}/oauth/token", endpoints::AUTH_SERVICE);
            let token = match post_json(&proxy_url, &url, &body, &[]).await {
                Ok(token) => token,
                Err(error) => return fail(flow, &error),
            };
            let access = token
                .get("accessToken")
                .and_then(Value::as_str)
                .unwrap_or("");
            if access.is_empty() {
                return fail(flow, "Kiro 没有返回 token");
            }
            let credentials = Credentials {
                access: access.to_string(),
                refresh: token
                    .get("refreshToken")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                expires: expiry(&token, "expiresIn"),
                method: "social".into(),
                login_provider: option.clone(),
                uid: option.clone(),
                region: "us-east-1".into(),
                profile: token
                    .get("profileArn")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                ..Default::default()
            };
            finish(flow, credentials).await
        }
        "builderid" | "awsidc" | "internal" => {
            let Some(issuer) = request.query.get("issuer_url").cloned() else {
                return fail(flow, "Kiro 登录页没有说明 AWS 登录地址");
            };
            let Some(region) = request.query.get("idc_region").cloned() else {
                return fail(flow, "Kiro 登录页没有说明 AWS 地区");
            };
            if !endpoints::is_aws_region(&region) {
                return fail(flow, "Kiro 登录页给的 AWS 地区不合法");
            }
            if !endpoints::is_https_url(&issuer) {
                return fail(flow, "Kiro 登录页给的 AWS 登录地址不是 https");
            }
            match start_aws(&option, &issuer, &region, aws_redirect, &proxy_url).await {
                Ok(authorize) => {
                    let provider = match option.as_str() {
                        "builderid" => "BuilderId",
                        "awsidc" => "Enterprise",
                        _ => "Internal",
                    }
                    .to_string();
                    with_flow(flow, |state| {
                        state.stage = "aws".into();
                        state.aws = Some(AwsPending {
                            region: region.clone(),
                            client_id: authorize.client_id,
                            client_secret: authorize.client_secret,
                            provider,
                            state: authorize.state,
                            verifier: authorize.verifier,
                            on_to_aws: authorize.url.clone(),
                            portal_asked: asked.clone(),
                        });
                    });
                    Response::redirect(authorize.url)
                }
                Err(error) => fail(flow, &error),
            }
        }
        "external_idp" => fail(
            flow,
            "公司自建身份提供商的浏览器登录暂不支持；请用 kiro-cli 登录后选择「导入 kiro-cli / Kiro IDE 登录态」",
        ),
        other => fail(
            flow,
            &format!("Kiro 登录页返回了不认识的登录方式（{other}）"),
        ),
    }
}

struct AwsAuthorize {
    url: String,
    state: String,
    verifier: String,
    client_id: String,
    client_secret: String,
}

async fn start_aws(
    option: &str,
    issuer: &str,
    region: &str,
    aws_redirect: &str,
    proxy_url: &str,
) -> Result<AwsAuthorize, String> {
    let body = json!({
        "clientName": "Kiro IDE",
        "clientType": "public",
        "scopes": endpoints::SCOPES,
        "grantTypes": ["authorization_code", "refresh_token"],
        "redirectUris": ["http://127.0.0.1/oauth/callback"],
        "issuerUrl": issuer,
    });
    let url = format!("{}/client/register", endpoints::oidc(region));
    let registration = post_json(proxy_url, &url, &body, &[]).await?;
    let client_id = registration
        .get("clientId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if client_id.is_empty() {
        return Err("AWS 没有注册出客户端".to_string());
    }
    let client_secret = registration
        .get("clientSecret")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let state = random_token(24);
    let verifier = random_token(48);
    let challenge = pkce_challenge(&verifier);
    let authorize = format!(
        "{}/authorize?response_type=code&client_id={}&redirect_uri={}&scopes={}&state={}&code_challenge={}&code_challenge_method=S256",
        endpoints::oidc(region),
        urlencode(&client_id),
        urlencode(aws_redirect),
        urlencode(&endpoints::SCOPES.join(",")),
        state,
        challenge
    );
    let _ = option;
    Ok(AwsAuthorize {
        url: authorize,
        state,
        verifier,
        client_id,
        client_secret,
    })
}

/// AWS 侧的 provider 名 → 本插件落的规范值（小写、可比较）。
fn login_provider(provider: &str) -> String {
    match provider {
        "BuilderId" => "builderid",
        "Enterprise" => "enterprise",
        "Internal" => "internal",
        other => other,
    }
    .to_string()
}

/// 规范值 → 账号名里的括号说明。
fn display_way(way: &str) -> &str {
    match way {
        "google" => "Google",
        "github" => "GitHub",
        "builderid" => "Builder ID",
        "enterprise" => "IAM Identity Center",
        "internal" => "Internal",
        other => other,
    }
}

fn expiry(token: &Value, field: &str) -> i64 {
    let seconds = token.get(field).and_then(Value::as_i64).unwrap_or(0);
    now_ms() + if seconds > 0 { seconds } else { 3600 } * 1000
}

/// 拿到凭证：问一次邮箱与套餐，落成待取的账号。
async fn finish(flow: &Arc<Mutex<Flow>>, mut credentials: Credentials) -> Response {
    let proxy_url = with_flow(flow, |state| {
        state
            .proxy
            .as_ref()
            .map(|proxy| proxy.to_url())
            .unwrap_or_default()
    });
    if credentials.profile.is_empty() {
        let region = credentials.region();
        if let Ok(profile) = super::refresh::lookup_profile(&credentials, &region, &proxy_url).await
        {
            credentials.profile = profile;
        }
    }
    if let Ok(payload) = usage::fetch_usage_limits(&credentials, &proxy_url).await {
        if let Some(email) = payload
            .pointer("/userInfo/email")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            credentials.account_id = email.to_string();
        }
        if let Some(title) = payload
            .pointer("/subscriptionInfo/subscriptionTitle")
            .and_then(Value::as_str)
        {
            credentials.plan = usage::plan_name(title);
        }
    }
    let name = if credentials.account_id.is_empty() {
        "Kiro 账号".to_string()
    } else if credentials.uid.is_empty() {
        credentials.account_id.clone()
    } else {
        format!(
            "{} ({})",
            credentials.account_id,
            display_way(&credentials.uid)
        )
    };
    let mut secret = json!({ "name": name });
    credentials.write_into(&mut secret);
    with_flow(flow, |state| {
        state.stage = "over".into();
        state.result = Some(Ok(secret));
        if let Some(abort) = state.abort.take() {
            abort.abort();
        }
    });
    logging::verbose("kiro", "浏览器登录完成");
    Response::html(
        200,
        page(true, "登录完成", "Kiro 账号已添加，可以关闭这个标签页了。"),
    )
}

fn fail(flow: &Arc<Mutex<Flow>>, message: &str) -> Response {
    with_flow(flow, |state| {
        state.stage = "over".into();
        if state.result.is_none() {
            state.result = Some(Err(message.to_string()));
        }
        if let Some(abort) = state.abort.take() {
            abort.abort();
        }
    });
    Response::html(200, page(false, "登录没有完成", message))
}

async fn post_json(
    proxy_url: &str,
    url: &str,
    body: &Value,
    headers: &[(String, String)],
) -> Result<Value, String> {
    let client = crate::http::control_client(None, proxy_url);
    let mut request = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(
            "User-Agent",
            endpoints::ide_user_agent(&super::local::hostname()),
        )
        .timeout(std::time::Duration::from_secs(30))
        .json(body);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("登录请求失败：{}", crate::http::describe(&error)))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        let mut detail = String::new();
        if let Ok(value) = serde_json::from_str::<Value>(&text) {
            detail = value
                .get("error_description")
                .and_then(Value::as_str)
                .or_else(|| value.get("message").and_then(Value::as_str))
                .or_else(|| value.get("error").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
        }
        return Err(format!(
            "登录被拒绝（HTTP {status}）{}",
            if detail.is_empty() {
                String::new()
            } else {
                format!("：{detail}")
            }
        ));
    }
    serde_json::from_str(&text).map_err(|_| "登录返回不是有效 JSON".to_string())
}
