//! AtomCode 的转发：**同方言透传**，本仓最薄的一层。
//!
//! ── 为什么这一层几乎什么都不做 ────────────────────────────────
//! 网关本身就是 OpenAI Chat Completions，所以客户端请求体几乎逐字节上行、
//! 上游的 SSE 帧逐字节下发。别家那套「按家翻译帧」在这里没有对应物 ——
//! 没有一条字段会在「没有发生的翻译」里丢掉，这正是走同方言上游的全部好处。
//! 剩下的只有格式本身表达不了的四件事：**身份、模型名映射、参数补全、
//! 以及静默失败的识别**（最后一条在 `errors.rs` 与 SSE 出口）。
//!
//! ── 只改三处，每处都有它非改不可的理由 ──────────────────────────
//!   1. `model`：客户端点的是**展示名**（目录里的 `GLM-5.3-Flash`），网关只认
//!      自己的 id（`glm5.3-flash`）。不做这一步，网关不会报错，它会回
//!      `HTTP 200 + 正文 参数错误`（见 `errors.rs` 的模块头）——
//!      一个「模型名没映射」的 bug 会表现为「参数不被接受」。
//!      同时剥掉账号前缀（`<accountId>/<model>`，多账号下同名模型的消歧手段）。
//!   2. `stream_options`：官方客户端**无条件**发
//!      `{"include_usage": true}`，网关据此才吐尾部 usage 帧。客户端没指定时补上
//!      （客户端显式指定就让它赢）；非流式路径删掉它（没有流可挂）。
//!   3. 其余（messages / tools / tool_choice / temperature / max_tokens /
//!      reasoning_effort / 图片分片）**一律不动**。
//!
//! ── User-Agent 是功能性的（再说一遍，因为它最容易被「清理」掉）────
//! 网关按 UA 决定是否索要闭源签名：自称 `atomcode/<ver>` 会立刻
//! `403 ATOMCODE_SIG_MISSING`。见 `endpoints::ADAPTER_USER_AGENT` 的实测表。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use crate::headers::Headers;
use serde_json::Value;

use agent2api_provider_sdk::ChatPlan;
use crate::errors::GatewayError;

use super::credentials::AtomcodeCredentials;
use super::endpoints::{ADAPTER_USER_AGENT, gateway_chat_url};
use super::models;

/// 账号前缀的分隔符（目录里同名模型的消歧形态：`<accountId>/<model>`）
const ACCOUNT_PREFIX_SEPARATOR: char = '/';

/// 剥掉账号前缀：`<account>/glm5.3-flash` → `glm5.3-flash`。
///
/// ⚠️ **只剥一层且只剥「已知账号 id」那一层**不能在这里做（本函数拿不到账号表），
/// 因此判据是「第一段看起来像账号 id」：目录里的模型名本身**不含** `/`
/// （网关的 id 形如 `glm5.3-flash` / `deepseek-flash`），所以出现 `/` 就说明
/// 那是我们自己的前缀。这条判据的代价写在这里：将来若网关开始下发带 `/` 的
/// 模型 id（如 `moonshotai/Kimi-K2-Instruct` 这种**含斜杠**的上游 id），
/// 本函数会误剥。届时改为「按调用方给的账号 id 精确剥」。
/// 现在不那样写是因为 `build_chat_request` 的 `account` 参数里没有账号 id
/// 的独立字段（见 `adapter.rs` 关于 `account` 形态的说明）。
pub fn strip_account_prefix(model: &str) -> &str {
    let trimmed = model.trim();
    match trimmed.split_once(ACCOUNT_PREFIX_SEPARATOR) {
        Some((_prefix, rest)) if !rest.trim().is_empty() => rest.trim(),
        _ => trimmed,
    }
}

/// 准备一次上游调用。
pub fn build_chat_request(
    account: &Value,
    body: &Value,
    _client_headers: &Headers,
) -> Result<ChatPlan, GatewayError> {
    let credentials = AtomcodeCredentials {
        id: String::new(),
        access_token: account
            .get("auth")
            .and_then(|auth| auth.get("accessToken"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string(),
        refresh_token: String::new(),
        expires_at: 0,
        user_id: String::new(),
        name: String::new(),
    };
    if credentials.access_token.is_empty() {
        return Err(GatewayError::with_status(
            401,
            "AtomCode 账号缺少访问令牌，无法转发（请重新登录）",
        ));
    }

    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if requested.is_empty() {
        return Err(GatewayError::with_status(400, "请求缺少 model 字段"));
    }
    // 客户端点的是展示名 / 带前缀名；网关只认自己的 id。
    let published = strip_account_prefix(requested);
    let upstream_model = models::upstream_id_for(published).unwrap_or_else(|| published.to_string());

    let mut payload = body.clone();
    if let Some(object) = payload.as_object_mut() {
        object.insert("model".to_string(), Value::String(upstream_model));
        let streaming = object.get("stream").and_then(Value::as_bool).unwrap_or(false);
        if streaming {
            // 官方客户端无条件带它，网关据此才吐尾部 usage 帧；
            // 客户端显式指定过就不覆盖。
            if !object.contains_key("stream_options") {
                object.insert(
                    "stream_options".to_string(),
                    serde_json::json!({ "include_usage": true }),
                );
            }
        } else {
            // 非流式路径没有流可挂 usage，留着只会让上游多回一个字段
            object.remove("stream_options");
        }
    } else {
        return Err(GatewayError::with_status(400, "请求体不是 JSON 对象"));
    }

    let headers: Vec<(String, String)> = vec![
        (
            "authorization".to_string(),
            format!("Bearer {}", credentials.access_token),
        ),
        ("accept".to_string(), "application/json".to_string()),
        // 功能性常量：自称官方客户端会触发 ATOMCODE_SIG_MISSING
        ("user-agent".to_string(), ADAPTER_USER_AGENT.to_string()),
    ];

    Ok(ChatPlan::chat(gateway_chat_url(), headers, payload))
}
