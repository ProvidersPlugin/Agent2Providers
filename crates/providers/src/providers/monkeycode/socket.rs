//! 复用 HTTP 客户端建立代理隧道，再切换为 WebSocket；不另开直连通道。

use agent2api_provider_protocol::WireProxy;
use tokio_tungstenite::tungstenite::{handshake, protocol::Role};
use tokio_tungstenite::WebSocketStream;

use crate::errors::GatewayError;
use crate::http::AccountRequest;

pub(super) type TaskSocket = WebSocketStream<reqwest::Upgraded>;

pub(super) async fn connect(
    request: tokio_tungstenite::tungstenite::http::Request<()>,
    proxy: Option<&WireProxy>,
) -> Result<TaskSocket, GatewayError> {
    let mut url = url::Url::parse(&request.uri().to_string())
        .map_err(|_| failed("任务流地址无效"))?;
    let scheme = match url.scheme() {
        "ws" => "http",
        "wss" => "https",
        _ => return Err(failed("任务流地址协议无效")),
    };
    url.set_scheme(scheme).map_err(|_| failed("任务流地址协议无效"))?;
    let key = request.headers().get("sec-websocket-key")
        .ok_or_else(|| failed("任务流握手缺少校验信息"))?;
    let expected = handshake::derive_accept_key(key.as_bytes());
    let timeouts = crate::timeouts::current();
    let mut builder = reqwest::Client::builder()
        .use_rustls_tls()
        .http1_only()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_millis(timeouts.connect_ms()));
    let proxy = crate::http::proxy_url(proxy, "direct");
    if !proxy.is_empty() && proxy != "direct" {
        builder = builder.proxy(reqwest::Proxy::all(&proxy)
            .map_err(|_| failed("任务流代理配置无效"))?);
    }
    let client = builder.build().map_err(|_| failed("无法创建任务流连接"))?;
    let response = client.get(url).headers(request.headers().clone())
        .with_account_user_agent().send().await.map_err(|_| failed("任务流连接失败，请检查账号代理和网络"))?;
    let status = response.status().as_u16();
    if status != 101 {
        return Err(GatewayError::with_status(
            if status == 401 || status == 403 { 401 } else { 502 },
            format!("MonkeyCode 任务流握手被拒（HTTP {status}）"),
        ));
    }
    let headers = response.headers();
    let contains = |name: &str, token: &str| headers.get_all(name).iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| value.split(',').any(|part| part.trim().eq_ignore_ascii_case(token)));
    if !contains("upgrade", "websocket") || !contains("connection", "upgrade")
        || headers.get("sec-websocket-accept").and_then(|value| value.to_str().ok()) != Some(expected.as_str())
        || headers.contains_key("sec-websocket-extensions") || headers.contains_key("sec-websocket-protocol")
    {
        return Err(failed("任务流握手校验失败"));
    }
    let upgraded = response.upgrade().await.map_err(|_| failed("任务流连接升级失败"))?;
    Ok(WebSocketStream::from_raw_socket(upgraded, Role::Client, None).await)
}

fn failed(message: &str) -> GatewayError {
    GatewayError::with_status(502, format!("MonkeyCode {message}"))
}
