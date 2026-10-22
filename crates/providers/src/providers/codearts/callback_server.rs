//! CodeArts 网页登录的**本机回调监听器**（一次登录一个，随机端口）。
//!
//! ── 为什么必须自己绑端口（不能挂网关端口）────────────────────
//! portal 的授权页**只认我们给的 `port`**，回调路径由它自己拼成
//! `http://127.0.0.1:<port>/oauth/callback`（见 `oauth::authorize_url`）。
//! 网关那版只能给网关自己的端口，于是同一端口上的多轮登录要靠一张待办表
//! 来分（回调里可能只带一个 `code`、不带任何配对信息）。
//!
//! 插件进程**每轮登录自己绑一个随机端口**，回调天然按端口归属 —— 表仍然保留
//! （见 `login.rs`），但「这是哪一轮」多了一层由端口表达的确定性。
//!
//! ── 迁移时改了什么：axum → 裸 tokio TCP ─────────────────────
//! 插件进程里没有 axum（也不必为此拉一个 Web 框架进来）：这里手写一个极小的
//! HTTP/1.1 请求行解析 —— 这条链路只需要「读请求头 → 看路径 → 投递 query →
//! 回一段 HTML」，不需要路由、中间件、提取器。
//!
//! ── 两次回调（这一家独有）───────────────────────────────────
//! portal 先带 `secret` + `redirect` 回来一次，并要求我们 **307 跳到它给的
//! redirect**（不跳，链路就停在半路、永远不会有 `code`）；随后再带 `code`
//! 回来。所以监听器**不能**在第一条回调后就收摊 —— `serve_one` 对带
//! `redirect` 的那一条回 307，并把 query 照常投递（`login.rs` 记下 secret）。
//!
//! ── 浏览器噪音怎么处理 ──────────────────────────────────────
//! 认不出授权材料（既没 `code` 也没 `secret`）就回 404 且不投递，监听器继续等。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic。

use std::net::Ipv4Addr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

use crate::logging;

use super::oauth::CALLBACK_PATH;

/// 回调路径（转导出，供 `login` 拼 `redirect_uri` 用）。
pub use super::oauth::CALLBACK_PATH as PATH;

/// 一次登录占用的 loopback 监听器。
pub struct CallbackListener {
    port: u16,
    shutdown: watch::Sender<bool>,
    /// 远程浏览器粘贴回调地址时，复用真实 HTTP 回调的投递通道。
    sender: std::sync::Mutex<Option<mpsc::UnboundedSender<String>>>,
    inbox: tokio::sync::Mutex<mpsc::UnboundedReceiver<String>>,
}

impl CallbackListener {
    /// 绑一个空闲 loopback 端口（IPv4 必须成功；IPv6 尽力而为）。
    ///
    /// 只绑回环：回调里带着一次性授权码，不给局域网开口子。
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
        // 浏览器把 localhost 解析成 ::1 时也要有人接。
        if let Ok(v6) = TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, port)).await {
            spawn_server(v6, sender.clone(), receiver);
        }
        logging::log(
            "[Login]",
            &format!("CodeArts OAuth 回调已监听 127.0.0.1:{port}{CALLBACK_PATH}"),
        );
        Ok(Self {
            port,
            shutdown,
            sender: std::sync::Mutex::new(Some(sender)),
            inbox: tokio::sync::Mutex::new(inbox),
        })
    }

    /// 这一轮占到的端口（拼 `auth_callback_url` 用）。
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 主动收摊：让监听任务退出、投递通道随之释放（幂等）。
    pub fn close(&self) {
        self.sender
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let _ = self.shutdown.send(true);
    }

    /// 等下一次**真回调**（认不出的噪音不会进到这里）。
    ///
    /// 返回 `None` = 通道关闭（监听器已 close / 任务已退出）。
    pub async fn next_callback(&self) -> Option<String> {
        let mut guard = self.inbox.lock().await;
        guard.recv().await
    }

    /// 从网关接口注入一条用户粘贴的回调 query。
    ///
    /// ── 这条为什么存在 ──────────────────────────────────────────
    /// 网关跑在另一台机器上（NAS / 服务器）时，浏览器跳的是**它自己**的
    /// 127.0.0.1，回调到不了插件这一侧。用户会把地址栏里那条地址改成网关
    /// 地址再走一遍（界面上就是这么教的），那时网关把整条 URL 交给插件 ——
    /// 这里投进同一个通道，与真实 HTTP 回调走完全同一条换证链。
    pub fn submit_callback(&self, query: &str) -> Result<(), String> {
        let parsed = parse_query(query);
        // 与 HTTP 回调同一条判据：带 code 或带 secret 才是真回调
        let has_code = parsed.get("code").is_some_and(|value| !value.trim().is_empty());
        let has_secret = parsed
            .get("secret")
            .is_some_and(|value| !value.trim().is_empty());
        if !has_code && !has_secret {
            return Err(
                "回调地址里既没有授权码也没有 secret：请把浏览器地址栏里那条回调地址完整粘贴过来"
                    .to_string(),
            );
        }
        let sender = self
            .sender
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .ok_or_else(|| "CodeArts 登录任务已结束，请重新发起".to_string())?;
        sender
            .send(query.to_string())
            .map_err(|_| "CodeArts 登录任务已结束，请重新发起".to_string())
    }
}

impl Drop for CallbackListener {
    fn drop(&mut self) {
        // 引用计数归零时兜底关一次（正常路径已经显式 close 过，这里是幂等兜底）。
        let _ = self.shutdown.send(true);
    }
}

/// 起一个只服务这一轮登录的 accept 循环，收到 shutdown 信号就退出。
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
                            // 一条连接一个任务：回调页会并发拉 favicon / preconnect，
                            // 串行处理会让真回调排在噪音后面。
                            let sender = sender.clone();
                            tokio::spawn(async move {
                                serve_one(stream, sender).await;
                            });
                        }
                        Err(error) => {
                            // accept 失败通常是瞬时资源问题（EMFILE）；不退出，
                            // 下一轮继续 —— 退出等于这一轮登录再也收不到回调。
                            logging::verbose("[Login]", &format!("CodeArts 回调 accept 失败：{error}"));
                        }
                    }
                }
                _ = shutdown.changed() => return,
            }
        }
    });
}

/// 处理一条连接：读请求头 → 认路径 → 投递 query → 回一页 HTML（或 307）→ 关。
async fn serve_one(mut stream: TcpStream, sender: mpsc::UnboundedSender<String>) {
    // 只读请求头（到空行为止），绝不读 body。上限 8 KiB。
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
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
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buffer.len() >= 8192 {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buffer);
    let request_line = head.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let _method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    };
    // 只认 portal 拼的那条路径，其余 404。
    if path != CALLBACK_PATH {
        respond(&mut stream, 404, "Not Found", "Not Found", None).await;
        return;
    }
    let parsed = parse_query(query);
    let has_code = parsed.get("code").is_some_and(|value| !value.trim().is_empty());
    let has_secret = parsed
        .get("secret")
        .is_some_and(|value| !value.trim().is_empty());
    if !has_code && !has_secret {
        respond(&mut stream, 404, "Not Found", "Not Found", None).await;
        return;
    }
    if sender.send(query.to_string()).is_err() {
        respond(
            &mut stream,
            410,
            "Login expired",
            "这一轮登录已经结束，请回到面板重新发起。",
            None,
        )
        .await;
        return;
    }
    // ── 第一次回调（带 secret + redirect）：必须 307 回 portal ────
    //
    // 不跳这一步，portal 的登录链路就停在半路、永远不会有 `code`。
    // 目的地只认华为云自己的站点（见 `trusted_redirect`）：这条路径是**免鉴权**
    // 的 loopback 端口，不校验就是把本机做成一个开放重定向。
    if has_secret && !has_code {
        match parsed.get("redirect").map(String::as_str) {
            Some(redirect) if trusted_redirect(redirect) => {
                redirect_to(&mut stream, redirect).await;
                return;
            }
            Some(_) => {
                respond(
                    &mut stream,
                    400,
                    "Redirect refused",
                    "回调要求跳转到的地址不是华为云站点，已拒绝。",
                    None,
                )
                .await;
                return;
            }
            // 没有 redirect = 用户手工粘回来的（浏览器那侧本来就到不了）→
            // 只回一页「已收到」，换证由 ticket 通道继续（见 `login.rs`）
            None => {}
        }
    }
    respond(
        &mut stream,
        200,
        "Login successful",
        "You can close this window now.",
        None,
    )
    .await;
}

/// 回调可以要求我们跳转的目的地：**只认华为云自己的站点**。
///
/// 判定用解析后的 host，不用字符串前缀 —— `https://huaweicloud.com.evil.tld/`
/// 这种写法能骗过朴素的后缀匹配之外的所有写法。
pub fn trusted_redirect(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "https" | "http") {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // 去掉用户信息（`user@host`）与端口
    let host_part = authority.rsplit('@').next().unwrap_or(authority);
    let host = host_part
        .split(':')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    host == "huaweicloud.com" || host.ends_with(".huaweicloud.com")
}

/// 查询串 → 键值对（**不做**百分号解码：`login.rs` 那边按需解）。
fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// 307 到 portal 给的地址（保留方法与 body，符合 portal 的链路要求）。
async fn redirect_to(stream: &mut TcpStream, location: &str) {
    let response = format!(
        "HTTP/1.1 307 Temporary Redirect\r\n\
         Location: {location}\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\
         \r\n"
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

/// 回一页 HTML 并关连接。
async fn respond(
    stream: &mut TcpStream,
    status: u16,
    title: &str,
    message: &str,
    extra: Option<(&str, &str)>,
) {
    let escaped = message.replace('<', "&lt;").replace('>', "&gt;");
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{title}</title>\
         <div style=\"font:16px/1.6 system-ui;padding:40px;text-align:center\">\
         <h2>{title}</h2><p>{escaped}</p></div>"
    );
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        410 => "Gone",
        _ => "OK",
    };
    let extra_headers = match extra {
        Some((name, value)) => format!("{name}: {value}\r\n"),
        None => String::new(),
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         {extra_headers}\
         Connection: close\r\n\
         \r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}
