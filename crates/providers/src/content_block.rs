//! 内容策略拦截的判定：**上游审核误报**的共用规则（各家共用）。
//!
//! 照搬网关 `core::providers::content_block`：HTTP 400 + 下列文案之一 = 内容策略
//! 拦截。
//!
//! ```text
//!   blocked by security policy
//!   unapproved channel
//!   illegal api invocation
//! ```
//!
//! ── 为什么这是「误报」而不是账号问题 ──────────────────────────
//! 上游按**逐字精确匹配**审核（不是语义审核），客户端注入的 system 模板句
//! 命中即整单拦截。此时账号本身健康：余额够、没限流、session 没死 —— 换账号
//! 再试是白扔另一个账号的额度（同一份 body 换谁发都会被拦）。所以分类结果
//! [`proto::ErrorClass::ContentBlocked`] 在编排层**不罚账号**，而是走「降级到
//! 中性提示词后同账号重试一次」。
//!
//! ── 大小写与状态码口径（与网关一致）───────────────────────────
//! 文案**大小写不敏感**；状态码只要求 `>= 400`（401 / 429 已在更前面被各自的
//! 分支接走）。

// 这一层是给「从网关搬来但还没接上」的家预备的共用规则，当前只有 Loomy
// 自己内联了一份同样的逻辑（它需要在 classify 里同时判业务码，合并会多一层
// 间接）。等下一批家搬进来就有消费方了。
#![allow(dead_code)]

use agent2api_provider_protocol as proto;
use serde_json::Value;

/// 内容策略拦截的文案特征（全部小写；匹配前把上游文案转小写）
const PATTERNS: &[&str] = &[
    "blocked by security policy",
    "unapproved channel",
    "illegal api invocation",
];

/// 客户端可见的**内容拦截提示**：附在 `上游返回 {status}: {上游原文}` 之后。
///
/// 与网关 `CONTENT_BLOCK_HINT` 逐字一致 —— 两处文案分叉会让同一件事在
/// 「内置家」与「插件家」的账号上呈现不同措辞。
pub const CONTENT_BLOCK_HINT: &str = "；内容被上游内容策略拒绝（这类拦截按逐字匹配，不是账号问题）：\
若为客户端 system 模板误报，可在设置页「通用 → 系统提示词」切到「替换」模式；\
若为对话内容本身触发审核，请调整后重试";

/// 是否命中内容策略拦截的文案特征。
pub fn matched(status: u16, error_body: &Value) -> bool {
    if status < 400 {
        return false;
    }
    let Some(message) = error_body.get("message").and_then(Value::as_str) else {
        return false;
    };
    let lowered = message.to_lowercase();
    PATTERNS.iter().any(|pattern| lowered.contains(pattern))
}

/// 分类兜底：命中内容拦截文案 → `ContentBlocked`（并补提示），否则 → `Fatal`。
pub fn classify_or_fatal(
    status: u16,
    error_body: &Value,
    message: String,
    upstream_code: Option<i64>,
) -> proto::ErrorClass {
    if matched(status, error_body) {
        return proto::ErrorClass::ContentBlocked {
            status,
            message: format!("{message}{CONTENT_BLOCK_HINT}"),
            upstream_code,
        };
    }
    proto::ErrorClass::Fatal {
        status,
        message,
        upstream_code,
    }
}
