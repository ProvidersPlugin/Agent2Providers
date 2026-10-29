use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use crate::errors::GatewayError;

const PORT: u16 = 9342;
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Target {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    url: String,
    web_socket_debugger_url: Option<String>,
}

fn is_chat(target: &Target) -> bool {
    if !matches!(target.kind.as_str(), "page" | "webview") {
        return false;
    }
    let Ok(url) = url::Url::parse(&target.url) else {
        return false;
    };
    ((url.scheme() == "chrome" || url.scheme() == "doubaowork")
        && url.host_str() == Some("doubaowork-chat"))
        || (url.scheme() == "https"
            && url.host_str() == Some("www.doubao.com")
            && (url.path() == "/chat" || url.path().starts_with("/chat/")))
}

pub async fn read() -> Result<Value, GatewayError> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .build()
        .map_err(|_| GatewayError::with_status(502, "无法读取豆包工作客户端"))?;
    let response = client.get(format!("http://127.0.0.1:{PORT}/json/list")).send().await
        .map_err(|_| GatewayError::with_status(400, "未连接到豆包工作客户端，请先登录并使用 --remote-debugging-port=9342 启动客户端；也可使用网页登录或粘贴 Cookie"))?;
    let targets: Vec<Target> = response
        .json()
        .await
        .map_err(|_| GatewayError::with_status(400, "豆包工作调试端口未返回有效页面列表"))?;
    if targets.len() > 256 {
        return Err(GatewayError::with_status(400, "豆包工作页面数量异常"));
    }
    let target = targets
        .into_iter()
        .find(is_chat)
        .ok_or_else(|| GatewayError::with_status(400, "豆包工作未打开已登录的对话页面"))?;
    let url = url::Url::parse(target.web_socket_debugger_url.as_deref().unwrap_or(""))
        .map_err(|_| GatewayError::with_status(400, "豆包工作调试连接无效"))?;
    if url.scheme() != "ws"
        || url.host_str() != Some("127.0.0.1")
        || url.port() != Some(PORT)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.path() != format!("/devtools/page/{}", target.id)
    {
        return Err(GatewayError::with_status(
            400,
            "豆包工作调试连接不属于本机客户端",
        ));
    }
    tokio::time::timeout(TIMEOUT, async {
        let (mut socket, _) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .map_err(|_| GatewayError::with_status(502, "连接豆包工作调试页面失败"))?;
        socket
            .send(Message::Text(
                json!({"id":1,"method":"Network.getAllCookies","params":{}})
                    .to_string()
                    .into(),
            ))
            .await
            .map_err(|_| GatewayError::with_status(502, "读取豆包工作登录态失败"))?;
        while let Some(message) = socket.next().await {
            let message =
                message.map_err(|_| GatewayError::with_status(502, "豆包工作调试连接已中断"))?;
            let Message::Text(text) = message else {
                continue;
            };
            let value: Value = serde_json::from_str(text.as_str())
                .map_err(|_| GatewayError::with_status(502, "豆包工作登录态响应无效"))?;
            if value["id"] != 1 {
                continue;
            }
            if !value["error"].is_null() {
                return Err(GatewayError::with_status(
                    400,
                    "豆包工作客户端拒绝读取登录态",
                ));
            }
            let cookies = value["result"]["cookies"]
                .as_array()
                .ok_or_else(|| GatewayError::with_status(400, "豆包工作客户端没有可用 Cookie"))?;
            let filtered: Vec<Value> = cookies
                .iter()
                .filter(|cookie| {
                    let domain = cookie["domain"]
                        .as_str()
                        .unwrap_or("")
                        .trim_start_matches('.');
                    matches!(domain, "doubao.com" | "www.doubao.com") && cookie["path"] == "/"
                })
                .cloned()
                .collect();
            let _ = socket.close(None).await;
            return Ok(json!({"cookies":filtered}));
        }
        Err(GatewayError::with_status(502, "豆包工作未返回登录态"))
    })
    .await
    .map_err(|_| GatewayError::with_status(504, "读取豆包工作客户端登录态超时"))?
}
