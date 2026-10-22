//! AutoClaw OAuth 回调的**登记端口**监听器 —— 「回调必须落在哪」的唯一实现点。
//!
//! ── 为什么不能绑随机端口 ────────────────────────────────────
//! Zai 的 OAuth 服务会拿 `redirect_uri` 去比对**这个 client 登记过的白名单**，
//! 而白名单里只有官方客户端那四个 loopback 端口：
//!
//! ```text
//!   ALL_PORTS = [18432, 19654, 19723, 53699]        ← app.asar 里逐字读出
//!   getZaiCallbackUri() = http://localhost:<port>/auth/callback-zai
//! ```
//!
//! 不在名单里的 `redirect_uri`（随机端口、`127.0.0.1`、路径不对的形态……）
//! 一律被拒，窗口里直接渲染 `{"detail":"Redirect URI not registered for this client"}`。
//! **Google 是另一套规则**（RFC 8252 的 loopback 宽松匹配，端口任意）。
//!
//! ── 因此这里做的事 ───────────────────────────────────────────
//! 登录开始时**临时**占用四个登记端口里的第一个空闲端口，用它的端口号拼
//! `navigate_uri`（见 [`super::oauth::navigate_uri`]），并**自己**收下落到这个
//! 端口上的回调（投递给流程表）—— 插件里没有「网关的回调路由」可转发。
//!
//! 端口在**登录结束**时释放：[`CallbackListener::close`] 或 `Drop` 时通知监听
//! 任务退出。
//!
//! ── 四个端口都被占（官方客户端在跑）时怎么办 ─────────────────
//! 返回 `Err(人话文案)`，由调用方决定：内嵌窗口形态下**仍然**把 `navigate_uri`
//! 指向第一个登记端口（白名单那关必须过），并让壳侧的内嵌窗口把这次导航截回
//! 网关的 `/auth/callback-*`（见 `src-tauri/src/login.rs` 的
//! `autoclaw_callback_forward`）—— 那条路上端口由谁监听都不影响我们拿到 code。
//!
//! ── 绑定口径 ────────────────────────────────────────────────
//! 只绑 **loopback**（IPv4 必须成功；IPv6 尽力而为）：
//!   - 与官方客户端同款（它 `app.listen(port, "127.0.0.1")`），不给局域网开口子；
//!   - IPv6 那一半是给「浏览器把 localhost 解析成 ::1」的环境兜底。
//!
//! 只接受两个已知的回调路径，其它路径 404 —— 这个端口是**借来的**，尽量少应答。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 本 crate 编译期禁止 unwrap/expect/panic。

use std::net::{Ipv4Addr, Ipv6Addr};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

use crate::logging;

use super::oauth::CALLBACK_PATH_PREFIX;

/// z.ai 给 AutoClaw 客户端登记的四个回调端口（顺序 = 尝试顺序）。
///
/// 改这个列表前先确认两件事：官方客户端 `app.asar` 里的 `ALL_PORTS` 变了、
/// 且 Zai 的 redirect_uri 白名单跟着变了 —— 少一个都会让登录在 authorize
/// 那一跳被拒（现象就是那个 `Redirect URI not registered`）。
pub(super) const REGISTERED_CALLBACK_PORTS: [u16; 4] = [18432, 19654, 19723, 53699];

/// 两个已知的回调路径（与 [`CALLBACK_PATH_PREFIX`] 同源，这里只做白名单匹配）。
const CALLBACK_PATHS: [&str; 2] = ["/auth/callback-zai", "/auth/callback-google"];

/// 一次登录期间占用的回调端口。
///
/// **只持有端口与关闭信号**：投递通道由调用方（`web_login` 的流程表）持有 ——
/// 这样「端口抢不到、靠壳侧改道 + 管理动作注入」的那条路与「插件自己收」共用
/// 同一个投递出口，收尾逻辑只有一份。
pub(super) struct CallbackListener {
    port: u16,
    shutdown: watch::Sender<bool>,
    /// 远程面板粘贴回调时复用真实 HTTP 回调的投递通道。
    sender: std::sync::Mutex<Option<mpsc::UnboundedSender<String>>>,
}

impl CallbackListener {
    /// 占一个登记端口并把落上来的回调投递给 `sender`（值 = `{vendor}|{query}`）。
    ///
    /// 四个端口都占不到时返回 `Err`：文案是**事实陈述**（「都被占用了」），
    /// 具体取舍由调用方决定（见模块头）。
    pub(super) async fn bind(sender: mpsc::UnboundedSender<String>) -> Result<Self, String> {
        for port in REGISTERED_CALLBACK_PORTS {
            let Ok(v4) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await else {
                continue;
            };
            let v6 = TcpListener::bind((Ipv6Addr::LOCALHOST, port)).await.ok();
            let (shutdown, receiver) = watch::channel(false);
            spawn_server(v4, sender.clone(), receiver.clone());
            if let Some(v6) = v6 {
                spawn_server(v6, sender.clone(), receiver);
            }
            logging::log(
                "[Login]",
                &format!("AutoClaw OAuth 回调已占用登记端口 {port}"),
            );
            return Ok(Self {
                port,
                shutdown,
                sender: std::sync::Mutex::new(Some(sender)),
            });
        }
        Err(format!(
            "AutoClaw 登录所需的本地回调端口（{}）都被占用（通常是 AutoClaw 桌面客户端在运行）",
            REGISTERED_CALLBACK_PORTS
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(" / ")
        ))
    }

    /// 这一轮占到的端口（拼 `navigate_uri` 用）。
    pub(super) fn port(&self) -> u16 {
        self.port
    }

    /// 从管理接口注入一条用户粘贴的回调（远程面板形态）。
    pub(super) fn deliver(&self, payload: &str) -> Result<(), ()> {
        let sender = self
            .sender
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .ok_or(())?;
        sender.send(payload.to_string()).map_err(|_| ())
    }

    /// 收摊：让监听任务退出、端口还回去。
    ///
    /// 与 Accio / CodeArts 的同名方法同一理由：等待回调的任务自己还握着一份
    /// 引用，只靠 `Drop` 会让端口占到超时。取消 / 超时 / 完成三条路径都要显式
    /// close（幂等）。
    pub(super) fn close(&self) {
        self.sender
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let _ = self.shutdown.send(true);
    }
}

impl Drop for CallbackListener {
    fn drop(&mut self) {
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
                            logging::verbose(
                                "[Login]",
                                &format!("AutoClaw 回调 accept 失败：{error}"),
                            );
                        }
                    }
                }
                _ = shutdown.changed() => return,
            }
        }
    });
}

/// 处理一条连接：读请求行 → 认路径 → 投递 → 回一页 HTML → 关。
async fn serve_one(mut stream: TcpStream, sender: mpsc::UnboundedSender<String>) {
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
    // 只认两个已知路径（这个端口是借来的，其余一概 404）。
    if !CALLBACK_PATHS.contains(&path) {
        respond(&mut stream, 404, "Not Found", "Not Found").await;
        return;
    }
    let vendor = path
        .strip_prefix(CALLBACK_PATH_PREFIX)
        .unwrap_or("")
        .to_string();
    let parsed = parse_query(query);
    let has_code = parsed
        .get("code")
        .is_some_and(|value| !value.trim().is_empty());
    let has_error = parsed
        .get("error")
        .is_some_and(|value| !value.trim().is_empty());
    // 认不出材料（favicon / preconnect / 标签页预览）就 404，监听器继续等。
    if !has_code && !has_error {
        respond(&mut stream, 404, "Not Found", "Not Found").await;
        return;
    }
    if sender.send(format!("{vendor}|{query}")).is_err() {
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
            "你可以关闭这个窗口了。",
        )
        .await;
    }
}

/// 查询串 → 键值对（不做百分号解码，调用方自己解）。
fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// 回一页 HTML 并关连接（状态码 200 只说明「授权信息收到了」，成败由面板轮询显示）。
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
