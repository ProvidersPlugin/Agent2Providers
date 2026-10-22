//! WorkBuddy 的内置模型清单与准入过滤（从网关 `core::models` 搬来）。
//!
//! ── 内置清单什么时候被用到 ──────────────────────────────────
//! 只有「还没成功刷过 `/v3/config`」时才用：进程刚起来、或上游一直取不到。
//! 网关那份目录以它作为初始态（`initial_state`），插件侧同样 —— 否则重启后
//! 到第一次远程刷新之间 `/v1/models` 是空的，客户端会以为这家没有模型。
//!
//! ── 准入过滤（`is_chat_model`）为什么在插件里 ────────────────
//! `/v3/config` 会下发补全 / 改写 / 内部工具 / 图像 / 视频 / 旧代条目，
//! 网关只服务对话模型。这套判据（九个排除前缀 + `supportsExtra` 真值 +
//! `maxOutputTokens >= 16000`）是**这一家上游的形态知识**，因此跟着 provider
//! 走。网关侧仍保留同一份实现（内置家还在用它），两边逐字一致。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

/// 下游可见模型白名单：空数组 = 不限制，全量放行。
///
/// 与网关 `MODEL_ALLOWLIST` 同值（空）。需要收窄时填模型 id —— 但注意插件侧
/// 改这一行要发版，而网关侧改常量也是发版，所以保持同一条口径即可。
pub const MODEL_ALLOWLIST: &[&str] = &[];

/// 非对话模型的排除前缀（网关 `CHAT_MODEL_EXCLUDE_PREFIXES` 的逐字副本）。
///
/// `default-1.` 会把内置清单里的 `default-1.1` / `default-1.2` 一起排除 ——
/// 这是 Node 版的既有行为（那两个是 Claude 的内部 id），移植时保持一致。
const CHAT_MODEL_EXCLUDE_PREFIXES: &[&str] = &[
    "completion-",
    "codewise-",
    "hunyuan-image",
    "hunyuan-3b",
    "hunyuan-7b",
    "deepseek-r1-0528",
    "deepseek-v3-0324",
    "kimi-k2-instruct",
    "default-1.",
];

/// JS 真值判定（`supportsExtra` 那一格用）。
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|value| value != 0.0 && !value.is_nan()),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// JS `String(x)`（`id` 这类字段的容错文本化，与网关 `shape::value_text` 同义）。
pub fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        other => other.to_string(),
    }
}

/// 对话模型判定：网关只服务对话模型，非对话项不进目录。
pub fn is_chat_model(model: &Value) -> bool {
    let id = model.get("id").map(value_text).unwrap_or_default();
    if CHAT_MODEL_EXCLUDE_PREFIXES
        .iter()
        .any(|prefix| id.starts_with(prefix))
    {
        return false;
    }
    if model.get("supportsExtra").map(js_truthy).unwrap_or(false) {
        return false;
    }
    let Some(max_output) = model.get("maxOutputTokens").and_then(Value::as_f64) else {
        return false;
    };
    max_output >= 16_000.0
}

/// 目录准入过滤：对话模型判定 + 白名单（空白名单全量放行）。
pub fn apply_filters(models: Vec<Value>) -> Vec<Value> {
    models
        .into_iter()
        .filter(is_chat_model)
        .filter(|model| {
            if MODEL_ALLOWLIST.is_empty() {
                return true;
            }
            let id = model.get("id").map(value_text).unwrap_or_default();
            MODEL_ALLOWLIST.contains(&id.as_str())
        })
        .collect()
}

/// 内置模型目录（id → 元数据），逐字段照抄网关 `builtin_models()`。
///
/// `credits` 字段为桌面端展示的单价，仅作参考。
pub fn builtin_models() -> Vec<Value> {
    json!([
        { "id": "auto", "name": "Auto", "vendor": "f", "credits": "", "maxOutputTokens": 32000, "maxInputTokens": 168000, "supportsToolCall": true, "supportsImages": true, "isDefault": true, "descriptionZh": "平衡效果与速度" },
        { "id": "default", "name": "Default", "vendor": "v", "credits": "x2.00 credits", "maxOutputTokens": 24000, "maxInputTokens": 200000, "supportsToolCall": true, "supportsImages": false },

        { "id": "deepseek-v4-pro", "name": "Deepseek-V4-Pro", "vendor": "f", "credits": "x0.16 credits", "maxOutputTokens": 50000, "maxInputTokens": 1000000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true, "descriptionZh": "DeepSeek 旗舰模型，支持 1M 上下文窗口" },
        { "id": "deepseek-v4-flash", "name": "Deepseek-V4-Flash", "vendor": "f", "credits": "x0.06 credits", "maxOutputTokens": 50000, "maxInputTokens": 1000000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true, "descriptionZh": "DeepSeek 旗舰模型，支持 1M 上下文窗口" },
        { "id": "deepseek-v4.1-flash", "name": "Deepseek-V4.1-Flash", "vendor": "f", "credits": "x0.03 credits", "maxOutputTokens": 50000, "maxInputTokens": 1000000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true, "descriptionZh": "DeepSeek 快速模型，支持 1M 上下文窗口" },
        { "id": "deepseek-v3-2-volc", "name": "DeepSeek-V3.2", "vendor": "f", "credits": "", "maxOutputTokens": 32000, "maxInputTokens": 96000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },

        { "id": "glm-5.1", "name": "GLM-5.1", "vendor": "e", "credits": "", "maxOutputTokens": 48000, "maxInputTokens": 200000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "glm-5.0", "name": "GLM-5.0", "vendor": "f", "credits": "", "maxOutputTokens": 48000, "maxInputTokens": 200000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "glm-5.0-turbo", "name": "GLM-5.0-Turbo", "vendor": "e", "credits": "", "maxOutputTokens": 48000, "maxInputTokens": 200000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "glm-5v-turbo", "name": "GLM-5v-Turbo", "vendor": "e", "credits": "", "maxOutputTokens": 38000, "maxInputTokens": 200000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "glm-4.7", "name": "GLM-4.7", "vendor": "f", "credits": "", "maxOutputTokens": 48000, "maxInputTokens": 200000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "glm-4.6", "name": "GLM-4.6", "vendor": "f", "credits": "", "maxOutputTokens": 32000, "maxInputTokens": 168000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true },
        { "id": "glm-4.6v", "name": "GLM-4.6V", "vendor": "f", "credits": "", "maxOutputTokens": 32000, "maxInputTokens": 128000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },

        { "id": "minimax-m2.5", "name": "MiniMax-M2.5", "vendor": "f", "credits": "", "maxOutputTokens": 48000, "maxInputTokens": 200000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "minimax-m2.7", "name": "MiniMax-M2.7", "vendor": "f", "credits": "", "maxOutputTokens": 48000, "maxInputTokens": 200000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "minimax-m3-play", "name": "MiniMax-M3", "vendor": "f", "credits": "x0.25 credits", "maxOutputTokens": 48000, "maxInputTokens": 512000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true, "descriptionZh": "原生多模态，擅长代码、智能体任务" },

        { "id": "kimi-k2.6", "name": "Kimi-K2.6", "vendor": "f", "credits": "", "maxOutputTokens": 32000, "maxInputTokens": 256000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "kimi-k2.5", "name": "Kimi-K2.5", "vendor": "f", "credits": "", "maxOutputTokens": 32000, "maxInputTokens": 256000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "kimi-k2-thinking", "name": "Kimi-K2-Thinking", "vendor": "f", "credits": "", "maxOutputTokens": 32000, "maxInputTokens": 256000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "kimi-k2-instruct-taiji", "name": "Kimi-K2", "vendor": "f", "credits": "", "maxOutputTokens": 8192, "maxInputTokens": 31000, "supportsToolCall": true, "supportsImages": true },

        { "id": "hy3-preview", "name": "Hy3 preview", "vendor": "j", "credits": "", "maxOutputTokens": 64000, "maxInputTokens": 192000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "hy3-preview-agent", "name": "Hy3 preview", "vendor": "j", "credits": "x0.04 credits", "maxOutputTokens": 64000, "maxInputTokens": 192000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "hunyuan-chat", "name": "Hunyuan-Turbos", "vendor": "j", "credits": "", "maxOutputTokens": 8192, "maxInputTokens": 128000, "supportsToolCall": true, "supportsImages": true, "descriptionZh": "腾讯自研的轻量、快速的通用模型" },
        { "id": "hunyuan-2.0-instruct", "name": "Hunyuan-2.0-Instruct", "vendor": "j", "credits": "", "maxOutputTokens": 16000, "maxInputTokens": 128000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "hunyuan-2.0-thinking", "name": "Hunyuan-2.0-Thinking", "vendor": "e", "credits": "", "maxOutputTokens": 24000, "maxInputTokens": 128000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },

        { "id": "default-1.1", "name": "Claude-3.7-Sonnet", "vendor": "e", "credits": "", "maxOutputTokens": 8192, "maxInputTokens": 200000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
        { "id": "default-1.2", "name": "Claude-4.0-Sonnet", "vendor": "e", "credits": "", "maxOutputTokens": 24000, "maxInputTokens": 200000, "supportsToolCall": true, "supportsImages": true, "supportsReasoning": true, "onlyReasoning": true },
    ])
    .as_array()
    .cloned()
    .unwrap_or_default()
}

/// 内置清单经准入过滤（网关 `initial_state` 的兜底分支）。
pub fn builtin_filtered() -> Vec<Value> {
    apply_filters(builtin_models())
}
