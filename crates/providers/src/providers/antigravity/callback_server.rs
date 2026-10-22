//! Antigravity 网页登录的**本机回调监听器**（一次登录一个，随机端口）。
//!
//! ── 为什么自绑端口而不是挂网关端口 ─────────────────────────
//! 网关那版把回调挂在网关自己的 `/oauth-callback` 上，靠 `state` 认回那一轮。
//! 插件侧自绑端口有两个好处：① 不依赖「网关那条路由还在」（插件与网关的
//! 版本可以独立演进）；② 端口天然表达「这一轮是谁的」—— 回调落到哪个监听器
//! 上就是哪一轮，`state` 逐字比对是第二道闸门（Google 会原样带回它）。
//!
//! 与 Accio / CodeArts 两份的差别只有一处：**路径不同**（`/oauth-callback`
//! 连字符，是 Google 授权页逐字回跳的那条，参考实现 `oauth_server.rs` 同款），
//! 其余「只读请求头、回完就关、认不出材料就 404 继续等」逐字一致。
//!
//! ── 浏览器噪音怎么处理 ──────────────────────────────────────
//! 打开授权页的那一类请求里，落到本端口上的第一个请求经常不是真回调
//! （preconnect / favicon / 标签页预览）。判据交给「带 code 或带 error」：
//! 认不出授权材料就回 **404 且不投递**，监听器继续等 —— 一次就收尾等于把
//! 真回调挡在门外。
//!
//! ── 端口什么时候还 ─────────────────────────────────────────
//! [`CallbackListener::close`] 或 `Drop` 时通知监听任务退出。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic。

use std::net::Ipv4Addr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

use crate::logging;

/// 回调路径（转导出，供 `login` 拼 `redirect_uri` 用）。
pub use super::oauth::CALLBACK_PATH as PATH;

/// 本文件内部用的短名（`PATH` 是对外导出的别名，读起来更清楚）。
use super::oauth::CALLBACK_PATH;

/// 一次登录占用的 loopback 监听器。
///
/// `port` 用来拼授权地址（必须先 bind 成功、再拿真实端口造 `redirect_uri`，
/// 否则回调 URL 与实际监听地址会不是同一个端口）；`inbox` 由
/// [`Self::next_callback`] 消费。
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
        // 浏览器把 localhost 解析成 ::1 时也要有人接（多数环境先试 IPv4，
        // 但 macOS / 双栈 Docker 上出现过只连 ::1 的情况）。拿不到不影响。
        if let Ok(v6) = TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, port)).await {
            spawn_server(v6, sender.clone(), receiver);
        }
        logging::log(
            "[Login]",
            &format!("Antigravity OAuth 回调已监听 127.0.0.1:{port}{CALLBACK_PATH}"),
        );
        Ok(Self {
            port,
            shutdown,
            sender: std::sync::Mutex::new(Some(sender)),
            inbox: tokio::sync::Mutex::new(inbox),
        })
    }

    /// 这一轮占到的端口（拼 `redirect_uri` 用）。
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 主动收摊：让监听任务退出、投递通道随之释放。
    ///
    /// ── 为什么「取消」必须有这一手，不能只靠 `Drop` ────────────
    /// 监听器被 `Arc` 共享着：登录表移除那条记录时，正在 `await` 回调的任务
    /// **自己还握着一份引用**，`Drop` 因此不会发生 —— 只靠 Drop 释放端口的话，
    /// 取消会退化成「等超时才还端口」。所以取消/超时/完成这三条收尾路径都要
    /// 显式 close 一次（幂等：`watch` 重复置位没有副作用）。
    /// close 之后 `next_callback` 会以 `None` 结束，等待方据此退出。
    pub fn close(&self) {
        self.sender
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let _ = self.shutdown.send(true);
    }

    /// 等下一次**真回调**（认不出的噪音不会进到这里）。
    ///
    /// 返回 `None` = 通道关闭（监听器已 close / 任务已退出），调用方按「这一轮
    /// 不会再有回调」处理，不要当成空 query 继续循环。
    pub async fn next_callback(&self) -> Option<String> {
        let mut guard = self.inbox.lock().await;
        guard.recv().await
    }

    /// 从接口注入一条用户粘贴的回调 query。
    ///
    /// 注入仍经过与真实 HTTP 回调相同的「带 code 或带 error」判定，
    /// 因此空地址和浏览器噪音不会消耗这一轮登录。
    pub fn submit_callback(&self, query: &str) -> Result<(), String> {
        let parsed = parse_query(query);
        let has_code = parsed
            .get("code")
            .is_some_and(|value| !value.trim().is_empty());
        let has_error = parsed
            .get("error")
            .is_some_and(|value| !value.trim().is_empty());
        if !has_code && !has_error {
            return Err("回调地址没有携带可用的 Google 授权信息".to_string());
        }
        let sender = self
            .sender
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .ok_or_else(|| "Antigravity 登录任务已结束，请重新发起".to_string())?;
        sender
            .send(query.to_string())
            .map_err(|_| "Antigravity 登录任务已结束，请重新发起".to_string())
    }
}

impl Drop for CallbackListener {
    fn drop(&mut self) {
        // 引用计数归零时兜底关一次（正常路径已经显式 close 过，这里是幂等兜底：
        // 忘记放进登录表、或表被整体清空时，端口不至于占到进程结束）。
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
            // Drop 早于任务启动时 `changed()` 不会返回 —— 先查一次当前值。
            if *shutdown.borrow() {
                return;
            }
            tokio::select! {
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _peer)) => {
                            // 一条连接一个任务：回调页会并发拉 favicon / preconnect，
                            // 串行处理会让真回调排在噪音后面（用户看到"授权成功"
                            // 之后还要等几秒才收下账号）。
                            let sender = sender.clone();
                            tokio::spawn(async move {
                                serve_one(stream, sender).await;
                            });
                        }
                        Err(error) => {
                            // accept 失败通常是瞬时资源问题（EMFILE）；不退出，
                            // 下一轮继续 —— 退出等于这一轮登录再也收不到回调。
                            logging::verbose(
                                "[Login]",
                                &format!("Antigravity 回调 accept 失败：{error}"),
                            );
                        }
                    }
                }
                _ = shutdown.changed() => return,
            }
        }
    });
}

/// 处理一条连接：读请求行 → 认路径 → 投递 query → 回一页 HTML → 关。
async fn serve_one(mut stream: TcpStream, sender: mpsc::UnboundedSender<String>) {
    // 只读请求头（到空行为止），绝不读 body。上限 8 KiB：浏览器的请求头
    // 一般在 1 KiB 内，给到 8 KiB 足以容纳长查询串，又不会被恶意大包拖住。
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
    // `GET /oauth-callback?code=...&state=... HTTP/1.1`
    let mut parts = request_line.split_whitespace();
    let _method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    };
    // 只有 `/oauth-callback` 一条路径（Google 授权时登记的 redirect_uri 的路径）
    if path != CALLBACK_PATH {
        respond(&mut stream, 404, "Not Found", "Not Found").await;
        return;
    }
    // 认不出材料（favicon / preconnect / 标签页预览）就回 404 让监听器继续等。
    let parsed = parse_query(query);
    let has_code = parsed
        .get("code")
        .is_some_and(|value| !value.trim().is_empty());
    let has_error = parsed
        .get("error")
        .is_some_and(|value| !value.trim().is_empty());
    if !has_code && !has_error {
        respond(&mut stream, 404, "Not Found", "Not Found").await;
        return;
    }
    if sender.send(query.to_string()).is_err() {
        respond(
            &mut stream,
            410,
            "Login expired",
            "这一轮登录已经结束，请回到面板重新发起。",
        )
        .await;
        return;
    }
    if has_error {
        let message = parsed.get("error").cloned().unwrap_or_default();
        respond(&mut stream, 200, "Login failed", &message).await;
    } else {
        respond(
            &mut stream,
            200,
            "Login successful",
            "You can close this window now.",
        )
        .await;
    }
}

/// 查询串 → 键值对（不做百分号解码：`login::finish` 自己解）。
fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// 回一页 HTML 并关连接。
///
/// ── 状态码为什么一律 200 ────────────────────────────────────
/// 这一步之后还要换证，成败此刻还不知道；页面只说明「授权信息收到了」。
/// 真正的失败由面板的轮询显示（与参考实现 `writeCallbackHTML` 同文案）。
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
    // 写失败无所谓：浏览器可能已经断开（标签页被关）。不在这里重试。
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}
