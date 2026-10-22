//! 数据面：一次转发的请求、流式头、错误分类。
//!
//! ── 这一层与网关 `core::providers::adapter` 的对应关系 ──────────
//! 网关原本的 `ChatRequestPlan`（URL + 头 + body + 响应协议）与
//! `UpstreamErrorClass`（四档分类）在这里各有一个**可序列化**的对应物。
//! 差别只有两处：
//!   1. 去掉生命周期与 `&'static str`（过线必须是 owned 数据）；
//!   2. 把「响应协议」从枚举改成 [`UpstreamKind`] —— 仍然封闭，因为它是
//!      网关侧翻译机的选择器，插件不能发明新的协议类型。
//!
//! ── 请求为什么分成 `chat` 与 `conversation` ──────────────────
//! 无状态家（一次 HTTP = 一次生成）与有状态家（建会话 → 工具循环 →
//! 完成）在网关里的编排完全不同：前者由网关发请求、只借插件的「怎么构造」，
//! 后者把整个发送过程交给插件。这个区分必须保留 —— 把它压成一种会让
//! 无状态家也背上会话状态机的成本。

use serde::{Deserialize, Serialize};

use crate::proxy::WireProxy;

/// 上游响应用哪套协议。
///
/// ── 为什么是封闭枚举（插件不能自定义）──────────────────────────
/// 这个值的唯一消费方是网关的翻译机选择（`upstream::translate`）。
/// 插件报一个网关不认识的协议，网关只能当 `Chat` 处理 —— 那会把
/// 非 OpenAI 的字节当 SSE 透传给客户端，症状是「客户端收到一堆乱码但
/// 状态码 200」。因此这里**显式拒绝**：网关侧反序列化失败即视为插件
/// 协议违规，报错并杀掉进程。
///
/// 加一家新协议 = 网关加一台翻译机 = 升 [`crate::PROTOCOL_VERSION`]。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamKind {
    /// OpenAI Chat Completions SSE（默认，绝大多数家）
    #[default]
    Chat,
    /// Anthropic Messages SSE
    Anthropic,
    /// AI SDK v5 风格 NDJSON（一行一个 JSON 事件，HTTP 恒 200；公开格式，
    /// 网关只认事件词表，见 `core::protocol::ai_sdk_ndjson`）
    Ndjson,
    /// Gemini `v1internal` 信封 + SSE（公开信封形态；请求体与鉴权由插件给）
    Gemini,
    /// SSE 混合帧：增量帧与全文快照帧混在一起（网关按「最后一份快照」收敛）
    ChatglmSnapshot,
}

/// 一次无状态转发请求（`chat` 方法）。
///
/// ── 谁构造请求 ──────────────────────────────────────────────
/// 插件。它拿到**客户端原始 body**（网关已做过角色归一、系统提示词、
/// 指纹脱敏三层处理，见网关 `upstream::payload`）与账号凭证，返回一个
/// 完整的 HTTP 请求描述 —— 与今天 `build_chat_request` 的职责逐字相同。
///
/// ── 为什么 `body` 是 `Value` 而不是已序列化的字节 ─────────────
/// 网关要做两件事需要结构化 body：改 `model`（备援名改写）与注入思考档位
/// （`reasoning_patch`）。让插件直接给字节会丢掉这两处能力。序列化在插件
/// 内部完成（各家的 JSON 形状规则不同，比如 WorkBuddy 要注入首条 system）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatRequest {
    /// 要发给哪一家（provider id）。
    pub provider: String,
    /// 承载这次请求的账号 id（`""` = 没指定，用插件自己的默认登录态）。
    pub account: String,
    /// 账号级出网代理（`None` = 跟随网关默认）。
    pub proxy: Option<WireProxy>,
    /// 客户端请求体（已过网关的三层处理）。
    pub body: serde_json::Value,
    /// 客户端入站头（小写键；只传插件可能用到的，见网关侧的白名单）。
    pub headers: std::collections::BTreeMap<String, String>,
    /// 客户端是否要流式。
    pub stream: bool,
    /// 网关解析出的**发送名**（备援改写之后、上游认识的那个名字）。
    ///
    /// ── 为什么由网关给而不是插件自己算 ──────────────────────────
    /// 发送名来自 `model_rules` 的映射链，那是**网关的用户配置**，插件
    /// 没有也不该有。插件拿它做两件事：拼 URL 里的模型路径、决定用哪套
    /// 请求信封。
    pub wire_model: String,
    /// 网关解析出的思考档位（映射上原样存着的字符串，可能为空）。
    ///
    /// 翻译成上游字段是插件的事（`reasoning.patch` 方法），这里只是把
    /// 用户配置的那条值带过来。
    pub reasoning: String,
}

/// 一次有状态转发请求（`conversation` 方法）。
///
/// ── 与 [`ChatRequest`] 的唯一差别 ────────────────────────────
/// 没有 `wire_model` / `reasoning` 的「构造请求」语义 —— 有状态家自己
/// 决定每一步发什么。多一个 `session`：客户端会话标识（用于上游的会话
/// 亲和性，例如 MonkeyCode 的任务复用）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConversationRequest {
    pub provider: String,
    pub account: String,
    pub proxy: Option<WireProxy>,
    pub body: serde_json::Value,
    pub headers: std::collections::BTreeMap<String, String>,
    pub stream: bool,
    /// 客户端会话标识（`x-session-id` 这类头，没有则空串）。
    #[serde(default)]
    pub session: String,
    /// 网关解析出的发送名（有状态家多数不需要，但 Trae / Qoder 这类
    /// 「一次请求仍是单模型」的家要用）。
    #[serde(default)]
    pub wire_model: String,
}

/// 流式回复的 `head` 帧。
///
/// 插件在**上游响应头到达后**立刻发这一帧，然后开始发 chunk。网关靠它
/// 决定状态码、要选哪台翻译机、以及要不要走「上游错误分类」那条路
/// （`status >= 400` 时网关会先把 body 收齐再分类，而不是透传）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StreamHead {
    /// 上游 HTTP 状态码。
    pub status: u16,
    /// 上游响应头（小写键）。插件**不应**过滤 `content-type`：
    /// 网关需要它判断是不是 SSE。
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    /// 这份响应用哪套协议。
    #[serde(default)]
    pub upstream: UpstreamKind,
}

/// 上游错误的四档分类。
///
/// ── 这四档是网关编排**唯一**认识的处置方式（不要加第五档）─────────
/// 网关 `upstream::provider_loop` 只按这四种行动：
///   - [`QuotaLimited`](Self::QuotaLimited) → 标记账号对该模型冷却 + 换账号；
///   - [`TokenExpired`](Self::TokenExpired) → 刷新凭证后**同一账号**重试一次；
///   - [`ContentBlocked`](Self::ContentBlocked) → **不罚账号**，换中性提示词
///     重试一次；
///   - [`Fatal`](Self::Fatal) → 原样透传给客户端。
///
/// 多出来的分类在网关侧没有对应分支，会**静默落进 Fatal** —— 于是「该轮换
/// 账号」的错误被当成终态发给用户。这就是为什么这里是封闭枚举。
///
/// 判错的后果同样是具体的：限额被当 Fatal → 账号永不轮换，每次都撞同一面
/// 墙；凭证过期被当 Fatal → 用户看到 401。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ErrorClass {
    /// 限额 / 风控：标记冷却并换账号。
    QuotaLimited {
        /// 上游给出的恢复时间戳（毫秒）；解析不出时 `None`（网关给兜底冷却）。
        reset_at: Option<i64>,
        /// 客户端可见的完整文案（插件拼好前缀，网关原样使用）。
        message: String,
        /// 上游业务码（进 `GatewayError.upstream_code` 与限额事件）。
        upstream_code: Option<i64>,
        /// HTTP 状态码。
        status: u16,
    },
    /// 凭证失效：刷新后同一账号重试一次。
    TokenExpired {
        /// 客户端可见的完整文案。
        message: String,
    },
    /// 内容策略拦截（审核误报）：不罚账号，换中性提示词重试。
    ContentBlocked {
        status: u16,
        message: String,
        upstream_code: Option<i64>,
    },
    /// 透传给客户端。
    Fatal {
        status: u16,
        message: String,
        upstream_code: Option<i64>,
    },
}

impl ErrorClass {
    /// 这一档要不要换账号（网关编排用）。
    pub fn rotates_account(&self) -> bool {
        matches!(self, Self::QuotaLimited { .. } | Self::TokenExpired { .. })
    }

    /// 给客户端看的文案。
    pub fn message(&self) -> &str {
        match self {
            Self::QuotaLimited { message, .. }
            | Self::TokenExpired { message }
            | Self::ContentBlocked { message, .. }
            | Self::Fatal { message, .. } => message,
        }
    }

    /// HTTP 状态码（`TokenExpired` 没有自己的状态码，恒按 401 呈现）。
    pub fn status(&self) -> u16 {
        match self {
            Self::QuotaLimited { status, .. }
            | Self::ContentBlocked { status, .. }
            | Self::Fatal { status, .. } => *status,
            Self::TokenExpired { .. } => 401,
        }
    }
}

/// 一次「可退避重试」的建议（`retry.advice` 方法的返回）。
///
/// ── 为什么是 `reason` 而不是整句日志 ─────────────────────────
/// 同一条建议有两个消费方：请求日志的重试链（要「为什么」）与终端控制台
/// （要完整一行，含「第 n/N 次」）。次数只有网关知道，所以插件只给原因，
/// 网关补进度 —— 两处各写一套文案迟早分叉。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetryAdvice {
    /// 退避时长（毫秒）。
    pub delay_ms: u64,
    /// 重试原因（provider 专属措辞，如「上游敏感词拦截（11-128）」）。
    pub reason: String,
}

/// 一次思考档位翻译的结果（`reasoning.patch` 方法的返回）。
///
/// ── 为什么是「要么一个字段、要么一个原因」而不是 `Option` ──────
/// 注入点要做两件事：改 body、**把结果记进详细日志**。「不注入」有四五种
/// 成因（这家不支持 / 客户端已指定 / 等级超出范围 / 该等级语义是关闭思考），
/// 对用户来说原因完全不同 —— 用 `Option` 会让日志只能写「未注入」，而那
/// 正是「我绑了为什么不生效」最难查的情形。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ReasoningPatch {
    /// 注入：把 body 顶层的 `field` 设成 `value`。
    Set {
        /// body 顶层的字段名（各家自己认识的那个键）。
        field: String,
        /// 要写入的值（字符串、布尔、对象都可能）。
        value: serde_json::Value,
    },
    /// 不注入（body 一个字节都不动）。`reason` 进详细日志。
    Skip {
        /// 给用户看的一句话原因。
        reason: String,
    },
}

/// `cooldown.models` 方法的返回：某账号在某个模型上被限额后，
/// 冷却键要不要连带覆盖别的模型名。
///
/// ── 为什么需要它 ────────────────────────────────────────────
/// 有些上游的限额是**按通道**记的（Cline 两池、Qoder 的套餐模型组）：
/// 一个模型被限额意味着同一通道的其它模型也不可用。网关只认模型名，
/// 不知道这层分组关系 —— 那是 provider 知识。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CooldownModels {
    /// 除请求的那个模型之外，还要一并冷却的模型名。
    #[serde(default)]
    pub models: Vec<String>,
}
