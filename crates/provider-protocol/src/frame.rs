//! 分帧：一行一个 JSON，双向对称。
//!
//! ── 线上形状 ────────────────────────────────────────────────
//! 两个方向共用一个信封：
//!
//! ```json
//! {"id": 7, "method": "chat", "params": {…}}        // 请求（任一方可发）
//! {"id": 7, "result": {…}}                          // 应答（成功）
//! {"id": 7, "error": {"message": "…", "code": "…"}}  // 应答（失败）
//! {"id": 7, "event": "head", "status": 200, …}       // 流式中间帧
//! {"id": 0, "event": "log", "level": "info", …}      // 通知（无应答）
//! ```
//!
//! ── 为什么是「一行一个 JSON」而不是长度前缀 ──────────────────
//! 与 magpie 同款，理由有三：
//!   1. 排障时可以直接 `tee` 到文件用 `jq` 看，不必先写解析器；
//!   2. 分帧的失败模式简单（没换行 = 还在发，超长 = 坏了）；
//!   3. 两端的实现都只有几十行，第三方照着写一个非 Rust 的插件也不难。
//!
//! 代价是 base64 的 33% 膨胀 —— 只作用于流式 chunk，控制面消息不含二进制。
//!
//! ── `id` 的空间 ─────────────────────────────────────────────
//! 双向对称意味着两边都在发请求。**每一方各自分配自己的 id**，互不冲突：
//! 收到的帧带 `method` 就是请求、带 `result`/`error` 就是应答，不看 id 的
//! 归属。`id = 0` 保留给通知（无应答），实现不得用 0 作为请求 id。

use std::collections::BTreeMap;
use std::io;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::limits::MAX_LINE_BYTES;

/// 一个线上帧。
///
/// ── 为什么是「一个宽结构 + 大量 Option」而不是 `enum` ──────────
/// 用 `#[serde(tag = "…")]` 的枚举会把「字段缺失」变成「反序列化失败」，
/// 而协议演进中最常见的情形恰恰是**新字段被旧实现读到**。宽结构 + `Option`
/// 让未知/缺失字段自然落到 `None`，旧实现读新帧不炸 —— 这是刻意留的兼容
/// 余量，不是偷懒。
///
/// 真正的形状判定由 [`Frame::kind`] 给出，见那里的说明。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Frame {
    /// 请求/应答关联 id。`0` = 通知（无应答）。
    #[serde(default)]
    pub id: i64,
    /// 请求方法名（带 `method` 即请求）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// 请求参数（仅请求有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
    /// 成功应答的载荷（仅应答有；JSON `null` 也是合法结果）。
    ///
    /// ── 为什么需要自定义反序列化（踩过的坑）────────────────────
    /// 默认的 `Option<Value>` 把「字段缺失」与「字段是 JSON null」都读成
    /// `None`，于是 `{"id":7,"result":null}` 会被 [`Frame::kind`] 判成
    /// `Unknown` —— 而「成功但没有返回值」正是流式调用收尾的形态（`abort`
    /// 的应答、`conversation` 的 `result: null`）。症状是网关永远等不到
    /// 收尾，客户端挂在那里。这里让「字段出现过」本身成为判据：出现过
    /// （哪怕值是 null）就是 `Some`。
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present_or_null")]
    pub result: Option<serde_json::Value>,
    /// 失败应答（仅应答有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
    /// 流式中间帧的事件名（`head` / `chunk` / `end`），见 [`FrameKind`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,

    /// 通知载荷：日志级别（`log` 事件用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    /// 通知载荷：日志正文（`log` 事件用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    /// 流式 `head` 帧：上游状态码。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// 流式 `head` 帧：上游响应头（小写键）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    /// 流式 `head` 帧：这份响应用哪套协议（决定网关选哪台翻译机）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// 流式 `chunk` 帧：base64 编码的载荷。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    /// 背压：`credit` 方法携带的字节数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit: Option<usize>,
}

/// 帧的语义类别（由 [`Frame::kind`] 判定，不是线上字段）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrameKind {
    /// 请求（带 `method`）。
    Request,
    /// 成功应答（带 `result`）。
    Reply,
    /// 失败应答（带 `error`）。
    Error,
    /// 流式中间帧（带 `event`）。
    Event,
    /// 认不出来 —— 对端发了既没有 method 也没有 result/error/event 的帧。
    Unknown,
}

/// 把「字段出现过」与「字段是 null」区分开（见 `Frame::result` 的说明）。
///
/// 只在字段真的出现在 JSON 里时才被调用（`#[serde(default)]` 管缺失），
/// 所以这里无条件 `Some`：null 是「应答的载荷是空」，不是「没有应答」。
fn present_or_null<'de, D>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(Some(value))
}

impl Frame {
    /// 判定这个帧的语义类别。
    ///
    /// ── 为什么判序是这样（改之前先读）──────────────────────────
    /// 1. `method` 优先：请求可能同时带 `event`（未来的流式请求），
    ///    按请求处理才不会把它错当中间帧；
    /// 2. `error` 先于 `result`：失败应答同时带 `result: null` 是合法的
    ///    （serde 的 `Option<Value>` 会把 JSON `null` 读成 `Some(Null)`），
    ///    先判 error 才能正确归类；
    /// 3. `event` 最后：它只出现在流式过程中。
    pub fn kind(&self) -> FrameKind {
        if self.method.is_some() {
            return FrameKind::Request;
        }
        if self.error.is_some() {
            return FrameKind::Error;
        }
        if self.result.is_some() {
            return FrameKind::Reply;
        }
        if self.event.is_some() {
            return FrameKind::Event;
        }
        FrameKind::Unknown
    }
}

/// 线上的错误体。
///
/// ── 与网关 `GatewayError` 的关系（重要）──────────────────────
/// 这不是 `GatewayError` 的镜像。`GatewayError` 是**给客户端看的最终形态**
/// （含 HTTP 状态码、`Retry-After`、上游业务码），而这里只表达「这次 RPC
/// 为什么失败」。把一个上游 429 变成 `GatewayError` 是插件侧的分类结果，
/// 它走的是 `chat` 的 `error` 字段（见 [`crate::chat::ErrorClass`]），
/// 不是这个结构。
///
/// 这个结构只用于**协议层故障**：方法不存在、参数解析失败、内部错误、
/// 超时。用户不该看到它的原文（多数是英文技术描述），网关会把它包一层。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireError {
    /// 人类可读的描述（给日志，不一定给用户）。
    pub message: String,
    /// 机器可识别的分类，见本结构的常量。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl WireError {
    /// 方法不存在（旧插件不实现新方法时的标准答复）。
    pub const NO_METHOD: &'static str = "no_method";
    /// 参数无法解析（形状不对、必填缺失）。
    pub const BAD_PARAMS: &'static str = "bad_params";
    /// 目标不存在（账号、台账、配置项）。插件据此区分「没有」与「坏了」——
    /// 前者是正常状态（新账号还没写过台账），后者该报错。
    pub const NOT_FOUND: &'static str = "not_found";
    /// 该 provider 不支持这个能力（例如对它调 `conversation`）。
    pub const UNSUPPORTED: &'static str = "unsupported";
    /// 调用被取消（对端发了 `abort`）。
    pub const ABORTED: &'static str = "aborted";
    /// 插件内部错误（panic 被捕获、依赖失败等）。
    pub const INTERNAL: &'static str = "internal";
    /// **本地前置条件不成立**：请求在还没出本机时就注定发不出去（今天的
    /// 用户：ZCode 活动套餐通道缺人机验证令牌，且本机没有 WebView 能铸）。
    ///
    /// ── 这一位为什么必须过线 ────────────────────────────────────
    /// 编排层对它的处置与其它错误**完全不同**：`provider_loop` 见到它会
    /// 「换下一个候选家顺延」而不是终止整份请求，选路会因此暂时绕开这一家。
    /// 判据必须是一个机器码 —— 靠错误文案前缀的话，插件改一个标点就会让
    /// 顺延静默失效（那正是 `readiness` 模块头写的教训）。
    ///
    /// 网关把这一位还原成 HTTP 语义上的 `556`
    /// （`providers::readiness::LOCAL_PRECONDITION_STATUS`）。
    pub const LOCAL_PRECONDITION: &'static str = "local_precondition";

    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), code: None }
    }

    pub fn with_code(message: impl Into<String>, code: impl Into<String>) -> Self {
        Self { message: message.into(), code: Some(code.into()) }
    }
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.code {
            Some(code) => write!(f, "{} ({})", self.message, code),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for WireError {}

/// 读一帧。
///
/// ── 错误分类（调用方据此决定「重启」还是「放弃」）──────────────
///   - `Ok(None)`：对端正常关闭（EOF）。父进程据此判定插件退出；
///   - `Err(kind == InvalidData)`：帧坏了（超长、不是合法 JSON）。**致命**，
///     调用方应当杀掉对端 —— 一个发出坏帧的进程不能信任后续字节边界；
///   - `Err(其他)`：IO 错误（管道断了）。同样致命。
///
/// ── 为什么超长要单独判 ──────────────────────────────────────
/// `read_until` 会一直读到换行，一个不发换行的子进程能让父进程分配任意大的
/// 缓冲区。这里边读边数，超了就报错，不等它结束。
///
/// 空行（对端多打了一个换行）被跳过而不是当成坏帧：那是无害的，判死会让
/// 一个只多写一个 `\n` 的第三方实现完全不可用。
pub async fn read_frame<R>(reader: &mut R) -> io::Result<Option<Frame>>
where
    R: AsyncBufRead + Unpin,
{
    loop {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let mut piece: Vec<u8> = Vec::new();
            let n = reader.read_until(b'\n', &mut piece).await?;
            if n == 0 && buf.is_empty() {
                return Ok(None); // 干净的 EOF
            }
            buf.extend_from_slice(&piece);
            if buf.len() > MAX_LINE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("帧超过 {MAX_LINE_BYTES} 字节上限"),
                ));
            }
            if piece.last() == Some(&b'\n') {
                break;
            }
            if n == 0 {
                // EOF 但缓冲区有半截数据：对端在帧中间死了
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "帧在中途被截断（对端未发完就关闭了）",
                ));
            }
        }
        // 去掉行尾（兼容 CRLF：Windows 上若有进程重定向过 stdout 就可能出现）
        while buf.last() == Some(&b'\n') || buf.last() == Some(&b'\r') {
            buf.pop();
        }
        if buf.is_empty() {
            continue; // 空行：跳过，读下一行
        }
        return serde_json::from_slice(&buf).map(Some).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("帧不是合法 JSON: {e}"))
        });
    }
}

/// 写一帧（自动追加换行并 flush）。
///
/// ── 为什么要 flush ──────────────────────────────────────────
/// 管道上没 flush 的字节会留在缓冲区里，对端一直等不到这一帧 —— 症状是
/// 「握手卡住」或「流式永远不开始」。因此这里**不用** `BufWriter`，直接写
/// 裸句柄并显式 flush：每帧一次 write syscall 的代价远小于「忘了 flush」
/// 这类故障的排查成本。
pub async fn write_frame<W>(writer: &mut W, frame: &Frame) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut line = serde_json::to_vec(frame)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("帧无法序列化: {e}")))?;
    if line.len() + 1 > MAX_LINE_BYTES {
        // ── 错误文案为什么要提到「请求体」─────────────────────────
        // 这个上限在真实世界里几乎只会被一件事撞到：`chat` / `conversation`
        // 的帧里带着完整请求体（含 base64 图片）。只说「帧超过 N 字节」
        // 会让用户与维护者都无从下手 —— issue #15 里那条 502 就是这样，
        // 报告者花了很久才定位到「是图片太多」。把最可能的原因写进错误，
        // 让这条信息本身可行动。
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "帧超过 {MAX_LINE_BYTES} 字节上限（{} 字节）：通常是请求体过大（含 base64 图片的上下文），请减少图片数量或缩小图片后重试",
                line.len()
            ),
        ));
    }
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await
}

/// 把一段字节编成 chunk 帧的 `data` 字段。
pub fn encode_chunk(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// 解码 chunk 帧的 `data` 字段。
pub fn decode_chunk(data: &str) -> io::Result<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(data).map_err(|e| {
        io::Error::new(io::ErrorKind::InvalidData, format!("chunk 不是合法 base64: {e}"))
    })
}

/// 包一层 `BufReader` 的标准构造（两边都按这个容量建）。
///
/// 1 MiB 是权衡：小于它会让大帧被拆成多次 syscall；大于它则每个连接多占
/// 内存（宿主最多数十条并发流，多 1 MiB/条就是几十 MB）。
pub fn buffered<R>(reader: R) -> BufReader<R>
where
    R: tokio::io::AsyncRead + Unpin,
{
    BufReader::with_capacity(1 << 20, reader)
}
