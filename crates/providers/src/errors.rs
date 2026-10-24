//! 网关错误类型的插件侧等价物。
//!
//! ── 为什么插件也要有一个「带状态码的错误」 ────────────────────
//! 从网关搬来的各家实现，内部一律返回 `Result<_, GatewayError>` —— 那个类型
//! 带 `status_code` / `upstream_code` / `code` 三个字段，各家的分支靠它们做
//! 区分（「401 ⇒ 重新登录」「400 + code=usage_not_configured ⇒ 未配置」）。
//! 全改成 `String` 会让那些分支退化成按文案匹配 —— 那是本项目反复拒绝的写法
//! （措辞一改就静默失效）。
//!
//! 所以这里复刻一份**同形**的最小类型：字段名、构造器、语义都与网关侧一致，
//! 只是不带 `thiserror` / HTTP 响应那些只属于网关的配套。
//!
//! ── 过线时怎么处理 ──────────────────────────────────────────
//! `Provider` 回调的错误通道是 `String`。控制面用 [`GatewayError::into_control_message`]
//! 携带 HTTP 状态及业务码，SDK 将其还原为 RPC 错误分类；其它路径的
//! [`GatewayError::into_message`] 只留文案，转发错误另走 `classify`。

/// 一次带状态的失败。
///
/// `code` 与 `upstream_code` 在插件内部用于分支；控制面通过
/// [`GatewayError::into_control_message`] 将它们一并传回网关。
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct GatewayError {
    /// 给用户看的完整文案。
    pub message: String,
    /// HTTP 语义的状态码（插件内部用来分支，过线后丢弃）。
    pub status_code: i32,
    /// 上游业务码（进日志与限额事件）。
    pub upstream_code: Option<i64>,
    /// 机器可识别的标记（`usage_not_configured` 这类）。
    pub code: Option<String>,
}

impl GatewayError {
    /// 通用构造：默认 500。
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status_code: 500,
            upstream_code: None,
            code: None,
        }
    }

    /// 指定状态码。
    pub fn with_status(status: i32, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status_code: status,
            upstream_code: None,
            code: None,
        }
    }

    /// 附一个机器可识别的标记。
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    /// 附上游业务码。
    pub fn upstream_code(mut self, code: i64) -> Self {
        self.upstream_code = Some(code);
        self
    }

    /// 附可选的上游业务码（`None` 时不动）。
    pub fn with_optional_code(mut self, code: Option<i64>) -> Self {
        if code.is_some() {
            self.upstream_code = code;
        }
        self
    }

    /// 过线形态（协议只传文案）。
    pub fn into_message(self) -> String {
        self.message
    }

    pub fn into_control_message(self) -> String {
        agent2api_provider_sdk::control_error(
            self.status_code,
            self.message,
            self.code,
            self.upstream_code,
        )
    }

    /// 只借文案（不消费）。
    pub fn message_of(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.message, formatter)
    }
}

impl std::error::Error for GatewayError {}

impl From<GatewayError> for String {
    fn from(error: GatewayError) -> Self {
        error.message
    }
}

impl From<String> for GatewayError {
    fn from(message: String) -> Self {
        Self::with_status(502, message)
    }
}

impl From<&str> for GatewayError {
    fn from(message: &str) -> Self {
        Self::with_status(502, message)
    }
}

/// 「该账号没有配置余额查询凭证」的错误码（与网关 `adapter::USAGE_NOT_CONFIGURED_CODE`
/// 逐字相同）。
///
/// ── 为什么必须逐字相同 ──────────────────────────────────────
/// 前端按 `code`（不是文案）判定「这是未配置、不是失败」，并把它渲染成中性
/// 提示。插件与网关用不同的码，这个判定就失效了 —— 用户会看到一个红色的
/// 「查询失败」，而实际只是「去设置里补一下凭证」。
pub const USAGE_NOT_CONFIGURED_CODE: &str = "usage_not_configured";

/// 「该账号没有配置余额查询凭证」的统一文案（`code` 同上）。
///
/// 会真正走到这里的是「余额接口另需一份查询凭证」的家（如 CatPaw 的网页会话
/// 凭证 `token2`、ZCode 的 Coding Plan JWT）。`field_hint` 说明去哪儿补，
/// 拼进 message；状态码用 **400**：这是前置条件缺失、用户自己能修。
pub fn usage_not_configured(provider_label: &str, field_hint: &str) -> GatewayError {
    GatewayError::with_status(
        400,
        format!(
            "该账号没有余额查询凭证（{provider_label} 的 {field_hint}），\
             请在账号设置里配置后再查询"
        ),
    )
    .with_code(USAGE_NOT_CONFIGURED_CODE)
}
