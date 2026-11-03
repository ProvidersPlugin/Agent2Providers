//! Trae HTTP 与流内业务错误分类。
//!
//! 4001 / 4023 表示模型与通道不匹配；1005 也可能只限当前通道。
//! 这些错误仅在首个内容帧之前尝试同模型的其它目录路由，耗尽后才分类限额。
//! 401 / 1001 / 20101 是凭据失效，4011 是限流，不能当成模型路由失败。

use serde_json::Value;

/// 错误类别。字符串形态与参考实现 `ErrKind.String()` 逐字一致 ——
/// 对拍报告与日志里两边要能直接对照。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// 成功
    None,
    /// 1005 / 4008：计划或模型配额不足（参考实现硬冷却 12 小时）
    PlanLimit,
    /// 429 / 9074：软限流（60 秒）
    SoftRate,
    /// 401：会话失效（禁用账号）
    SessionDead,
    /// 404：短冷却 60 秒、不累计错误数
    NotFound,
    /// 5xx
    Server,
    /// 其它 4xx
    Client,
    /// 413 或过大文案：请求级问题，**不冷却账号**
    InputTooLarge,
    /// 流内 4001：模型在当前 function 通道不可用，请求级、**不冷却也不累计**
    ModelUnavailable,
}

impl ErrorKind {
    /// 分类名（日志与排障用）。
    ///
    /// `#[allow(dead_code)]`：本家当前的日志走的是 `punish` 里那几句固定文案，
    /// 没有一处直接打这个字符串。留着它是因为**它是分类表的自述**（新增一档
    /// 时不会漏），且下一批要接的请求日志会用到。
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::PlanLimit => "plan_limit",
            Self::SoftRate => "soft_rate",
            Self::SessionDead => "session_dead",
            Self::NotFound => "not_found",
            Self::Server => "server",
            Self::Client => "client",
            Self::InputTooLarge => "input_too_large",
            Self::ModelUnavailable => "model_unavailable",
        }
    }

    /// 本仓 `UpstreamErrorClass` 的对应档位放在 M4（转发那层）做映射，
    /// 这里刻意**不认识**网关的错误类型 —— 分类知识只写一份。
    #[allow(dead_code)]
    pub fn is_request_level(self) -> bool {
        matches!(self, Self::InputTooLarge | Self::ModelUnavailable)
    }
}

/// 「输入过大」的子串词表（大小写不敏感；中文原样匹配）。
///
/// 这条判定的意义不是文案好看，而是**别冷却健康账号**：同一个超大 body
/// 换任何账号都会被拒，把它算成账号错误会连着拖垮整条链。
const INPUT_TOO_LARGE_MARKERS: [&str; 12] = [
    "too long",
    "too many tokens",
    "context length",
    "maximum context",
    "context window",
    "request entity too large",
    "input too large",
    "输入过长",
    "内容过长",
    "上下文过长",
    "上下文长度",
    "超出模型上限",
];

pub fn msg_indicates_input_too_large(message: &str) -> bool {
    let lower = message.to_lowercase();
    INPUT_TOO_LARGE_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

/// 流内业务码里"配额/计划不足"的那几个（与非 2xx 路径共用同一张表，
/// 免得同一个码在两条路径上得到两种语义）。
pub const PLAN_LIMIT_CODES: [i64; 2] = [1005, 4008];

/// 模型在当前 function 不可用。
pub fn is_model_mismatch_code(code: i64) -> bool {
    matches!(code, 4001 | 4023)
}

pub fn retry_route(status: u16, code: Option<i64>, message: &str) -> bool {
    !matches!(status, 401 | 403 | 429)
        && code.is_some_and(|code| matches!(code, 4001 | 4023 | 1005))
        && !msg_indicates_input_too_large(message)
}

/// 流内 `event: error` 的分类。
///
/// 顺序敏感：`4001` 配 "prompt is too long…" 仍归**过大**（那是输入问题，
/// 不是通道问题），所以过大判定在通道判定之前。
pub fn stream_error_kind(code: i64, message: &str) -> ErrorKind {
    if PLAN_LIMIT_CODES.contains(&code) {
        return ErrorKind::PlanLimit;
    }
    if msg_indicates_input_too_large(message) {
        return ErrorKind::InputTooLarge;
    }
    if is_model_mismatch_code(code) {
        return ErrorKind::ModelUnavailable;
    }
    if matches!(code, 1001 | 401 | 20101) {
        return ErrorKind::SessionDead;
    }
    if code == 4011 {
        return ErrorKind::SoftRate;
    }
    ErrorKind::Client
}

/// HTTP 状态 + 响应体 → 分类。判定顺序照参考实现 v0.12.51 的注释：
/// 413 提到最顶，避免 body 恰好带 "1005…plan" 字样时被宽松的 plan 匹配
/// 劫持成 12 小时硬冷却。
pub fn classify(status: u16, body: &str) -> ErrorKind {
    let lower = body.to_lowercase();
    if status == 413 {
        return ErrorKind::InputTooLarge;
    }
    if body.contains("\"code\":1005") || (body.contains("1005") && lower.contains("plan")) {
        return ErrorKind::PlanLimit;
    }
    if body.contains("\"code\":4008") {
        return ErrorKind::PlanLimit;
    }
    if (400..500).contains(&status) && msg_indicates_input_too_large(body) {
        return ErrorKind::InputTooLarge;
    }
    if status == 401 {
        // 参考实现这里遍历了一批"会话失效"文案标记，但无论命不命中都返回
        // 同一个类别 —— 那个循环是空转的，这里直接给结论。
        return ErrorKind::SessionDead;
    }
    if status == 429 {
        return ErrorKind::SoftRate;
    }
    if status == 404 {
        return ErrorKind::NotFound;
    }
    if status >= 500 {
        return ErrorKind::Server;
    }
    // 通道不可用放在账号级状态（401/429/404）之后兜底：鉴权失败永远优先按会话失效处理。
    if (400..500).contains(&status) && body.contains("\"code\":4001") {
        return ErrorKind::ModelUnavailable;
    }
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let (code, message) = parsed.as_ref().map(error_event_fields).unwrap_or_default();
    if matches!(code, 1001 | 401 | 20101) {
        return ErrorKind::SessionDead;
    }
    if code != 0 {
        let kind = stream_error_kind(code, &message);
        if kind != ErrorKind::Client {
            return kind;
        }
    }
    if status >= 400 {
        return ErrorKind::Client;
    }
    ErrorKind::None
}

/// 目录与流内错误兼容数字 / 数字字符串及外层 error 包裹。
pub fn error_event_fields(data: &Value) -> (i64, String) {
    let detail = data
        .get("error")
        .filter(|error| error.is_object())
        .unwrap_or(data);
    let code = detail
        .get("code")
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
        .unwrap_or(0);
    let message = detail
        .get("message")
        .or_else(|| detail.get("msg"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    (code, message)
}
