//! CatPaw 网页登录的**本机回调监听器**（一次登录一个随机端口）。
//!
//! ── 为什么不再挂在网关端口上 ────────────────────────────────
//! 网关那版把回调挂在 `127.0.0.1:<网关端口>/api/session/login/catpaw-callback`
//! （理由是「上游的 redirect 白名单只校验是不是 loopback，不要求端口随机」）。
//! 插件里没有那个端口 —— 网关的 HTTP 服务在**另一个进程**里，插件拿不到它，
//! 也不该让插件去要求网关暴露一条路由。所以这里自绑一个随机 loopback 端口，
//! 把它的地址放进 `redirect` 参数。
//!
//! 白名单只校验「是不是 loopback」，所以随机端口同样合规（实测传
//! `127.0.0.1:1` 也照样 302）。
//!
//! ── 上游的 POST 与浏览器噪音 ────────────────────────────────
//! 上游的 `login-callback` 页面把 `{token, state}` **表单 POST** 到这个地址。
//! 所以这里必须读 body（与 Trae 的 GET 回调不同），并且要认
//! `application/x-www-form-urlencoded` 与 JSON 两种形态 —— 上游用表单，
//! 我们自己测的时候用 JSON。
//!
//! 认不出 token 的请求回 404 且**不投递**：浏览器会顺手拉 favicon / preconnect，
//! 一次就收尾等于把真回调挡在门外。
//!
//! ── 与 Trae 的 callback_server 的关系 ───────────────────────
//! 两份都是「一次性 loopback 监听器」，但协议细节不同（GET vs POST、
//! 查询串 vs 表单体、`/authorize` vs `/catpaw/callback`），所以各写一份 ——
//! 合成一个带回调函数的通用件会让两边都多一层间接，而它们各自只有 200 行。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic。

use std::net::Ipv4Addr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

use crate::logging;

/// 回调路径。
///
/// 上游不校验路径（实测任意路径都放行），取一个带 provider 前缀的名字，
/// 免得与将来的其它回调撞车。
pub const CALLBACK_PATH: &str = "/catpaw/callback";

/// 请求体上限（登录态是一小段 token；8 KiB 足够，又不至于被大包拖住）。
const MAX_BODY: usize = 8192;

/// 一次登录占用的 loopback 监听器。
pub struct CallbackListener {
    port: u16,
    shutdown: watch::Sender<bool>,
    sender: std::sync::Mutex<Option<mpsc::UnboundedSender<String>>>,
    inbox: tokio::sync::Mutex<mpsc::UnboundedReceiver<String>>,
}

impl CallbackListener {
    /// 绑一个空闲 loopback 端口（IPv4 必须成功；IPv6 尽力而为）。
    pub async fn bind() -> Result<Self, String> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|error| format!("绑定本机回调端口失败：{error}"))?;
        let port = listener
            .local_addr()
            .map_err(|error| format!("读不到回调端口：{error}"))?
            .port();
        let (shutdown, receiver) = watch::channel(false);
        let (sender, inbox) = mpsc::unbounded_channel();
        spawn_server(listener, sender.clone(), receiver.clone());
        // 浏览器把 localhost 解析成 ::1 时也要有人接。拿不到不影响。
        if let Ok(v6) = TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, port)).await {
            spawn_server(v6, sender.clone(), receiver);
        }
        logging::log(
            "[Login]",
            &format!("CatPaw 登录回调已监听 127.0.0.1:{port}{CALLBACK_PATH}"),
        );
        Ok(Self {
            port,
            shutdown,
            sender: std::sync::Mutex::new(Some(sender)),
            inbox: tokio::sync::Mutex::new(inbox),
        })
    }

    /// 这一轮占到的端口（拼 `redirect` 用）。
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 主动收摊：让监听任务退出、投递通道随之释放。
    pub fn close(&self) {
        self.sender
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let _ = self.shutdown.send(true);
    }

    /// 等下一次**真回调**（认不出材料的噪音不会进到这里）。
    pub async fn next_callback(&self) -> Option<String> {
        let mut guard = self.inbox.lock().await;
        guard.recv().await
    }
}

impl Drop for CallbackListener {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

/// 起一个只服务这一轮登录的 accept 循环。
fn spawn_server(
    listener: TcpListener,
    sender: mpsc::UnboundedSender<String>,
    mut shutdown: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        loop {
            if *shutdown.borrow() {
                return;
            }
            tokio::select! {
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _peer)) => {
                            let sender = sender.clone();
                            tokio::spawn(async move {
                                serve_one(stream, sender).await;
                            });
                        }
                        Err(error) => {
                            // accept 失败通常是瞬时资源问题（EMFILE）；不退出，
                            // 下一轮继续 —— 退出等于这一轮登录再也收不到回调。
                            logging::verbose("[Login]", &format!("CatPaw 回调 accept 失败：{error}"));
                        }
                    }
                }
                _ = shutdown.changed() => return,
            }
        }
    });
}

/// 处理一条连接：读请求行 + 头 + body → 取 token → 投递 → 回一页 HTML。
async fn serve_one(mut stream: TcpStream, sender: mpsc::UnboundedSender<String>) {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let mut header_end = None;
    loop {
        let read = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream.read(&mut chunk),
        )
        .await
        {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(read)) => read,
            Ok(Err(_)) => return,
        };
        buffer.extend_from_slice(&chunk[..read]);
        if header_end.is_none() {
            if let Some(index) = find_header_end(&buffer) {
                header_end = Some(index);
            }
        }
        // 头部齐了且 body 也够长（或已经超上限）就停
        if let Some(index) = header_end {
            if let Some(length) = content_length(&buffer[..index]) {
                if buffer.len() >= index + 4 + length {
                    break;
                }
            } else if buffer.len() > index + 4 {
                break;
            }
        }
        if buffer.len() >= MAX_BODY {
            break;
        }
    }
    let Some(index) = header_end.or_else(|| find_header_end(&buffer)) else {
        respond(&mut stream, 404, "Not Found", "Not Found").await;
        return;
    };
    let head = String::from_utf8_lossy(&buffer[..index]).into_owned();
    let request_line = head.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    };
    if path != CALLBACK_PATH {
        respond(&mut stream, 404, "Not Found", "Not Found").await;
        return;
    }
    // GET（查询串）与 POST（表单 / JSON）两种都认：上游用 POST 表单，
    // 手工粘贴时可能是查询串。
    let body = String::from_utf8_lossy(&buffer[index + 4..]).into_owned();
    let token = if method.eq_ignore_ascii_case("POST") {
        token_from_body(&body)
    } else {
        token_from_query(query)
    };
    if token.is_empty() {
        respond(&mut stream, 404, "Not Found", "Not Found").await;
        return;
    }
    if sender.send(token).is_err() {
        respond(
            &mut stream,
            410,
            "Login expired",
            "这一轮登录已经结束，请回到面板重新发起。",
        )
        .await;
        return;
    }
    respond(&mut stream, 200, "Login successful", "You can close this window now.").await;
}

/// 找 `\r\n\r\n` 的位置（头部结束处）。
fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

/// 从头部里读 `Content-Length`。
fn content_length(head: &[u8]) -> Option<usize> {
    let text = String::from_utf8_lossy(head);
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse::<usize>().ok();
        }
    }
    None
}

/// 从 POST body 里取 `token`（表单或 JSON 两种形态）。
fn token_from_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.starts_with('{') {
        // JSON 形态：`{"token":"…","state":"…"}`
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(token) = value.get("token").and_then(serde_json::Value::as_str) {
                return token.trim().to_string();
            }
        }
        return String::new();
    }
    // 表单形态：`token=…&state=…`
    token_from_query(trimmed)
}

/// 从查询串里取 `token`。
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

/// 回一页 HTML 并关连接。
async fn respond(stream: &mut TcpStream, status: u16, title: &str, message: &str) {
    let escaped = message.replace('<', "&lt;").replace('>', "&gt;");
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{title}</title>\
         <div style=\"font:16px/1.6 system-ui;padding:40px;text-align:center\">\
         <h2>{title}</h2><p>{escaped}</p></div>"
    );
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        410 => "Gone",
        _ => "OK",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}
