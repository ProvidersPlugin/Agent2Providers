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

/// 读本机客户端的登录态：先试调试端口（最新、需要客户端带参数启动），
/// 不行就直接读客户端的 Cookie 数据库（**不需要调试端口**，见 `cookies_db`）。
///
/// ── 顺序为什么是「调试端口优先」──────────────────────────────
/// 客户端运行时，最新的 Cookie 先落内存、再慢慢写回 SQLite；调试端口读到的是
/// 内存里那一份，所以只要它有，就是最准的。没有调试端口时客户端会立刻拒绝
/// 连接（本机端口），回落到库文件几乎不增加等待。
pub async fn read() -> Result<Value, GatewayError> {
    match read_cdp().await {
        Ok(value) => Ok(value),
        Err(cdp_error) => match super::cookies_db::read() {
            Ok(value) => Ok(value),
            Err(disk_error) => Err(GatewayError::with_status(
                disk_error.status_code,
                format!("{}；{}", disk_error.message, cdp_error.message),
            )),
        },
    }
}

/// 只走调试端口（客户端带参数启动时）。给「已有快照、但客户端正好开着」的
/// 账号取最新会话用 —— 这条路径不该再摸硬盘上的库文件。
pub(super) async fn read_cdp() -> Result<Value, GatewayError> {
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
