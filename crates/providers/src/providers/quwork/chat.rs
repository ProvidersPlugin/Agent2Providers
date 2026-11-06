use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ChatPlan, PluginCtx};
use serde_json::{json, Value};

use super::{client, endpoints, models, refresh};

pub async fn build(ctx: &PluginCtx, request: proto::ChatRequest) -> Result<ChatPlan, String> {
    let catalog = models::current(ctx).await;
    let row = catalog.model(&request.wire_model).ok_or_else(|| {
        agent2api_provider_sdk::local_precondition("QuWork 当前目录没有这个模型，请先刷新模型列表")
    })?;
    let token = refresh::model_token(ctx, &request.account).await?;
    let mut body = request.body;
    let object = body
        .as_object_mut()
        .ok_or("QuWork 请求体必须是 JSON 对象")?;
    object.insert("model".into(), json!(request.wire_model));
    object.insert("stream".into(), json!(request.stream));
    if request.stream {
        let options = object.entry("stream_options").or_insert_with(|| json!({}));
        if let Some(options) = options.as_object_mut() {
            options.insert("include_usage".into(), json!(true));
        }
    } else {
        object.remove("stream_options");
    }
    let compat = row.get("compat").unwrap_or(&Value::Null);
    if compat.get("supportsStore").and_then(Value::as_bool) == Some(false) {
        object.remove("store");
    }
    if compat.get("maxTokensField").and_then(Value::as_str) == Some("max_tokens") {
        if let Some(limit) = object.remove("max_completion_tokens") {
            object.entry("max_tokens").or_insert(limit);
        }
    }
    let deepseek = compat.get("thinkingFormat").and_then(Value::as_str) == Some("deepseek");
    let explicit = crate::reasoning::read_client_level(&body);
    if deepseek {
        let level = explicit.as_deref().unwrap_or(&request.reasoning);
        if !level.is_empty() && body.get("thinking").is_none_or(Value::is_null) {
            let levels = row.get("reasoningEfforts").and_then(Value::as_object);
            let normalized = if level == "none" { "off" } else { level };
            if let Some(wire) = levels
                .and_then(|levels| levels.get(normalized))
                .and_then(Value::as_str)
            {
                if normalized == "off" {
                    body["thinking"] = json!({ "type": "disabled" });
                    if let Some(object) = body.as_object_mut() {
                        object.remove("reasoning_effort");
                    }
                } else {
                    body["thinking"] = json!({ "type": "enabled" });
                    body["reasoning_effort"] = json!(wire);
                }
            }
        }
    }
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            if compat.get("supportsDeveloperRole").and_then(Value::as_bool) == Some(false)
                && message.get("role").and_then(Value::as_str) == Some("developer")
            {
                message["role"] = json!("system");
            }
            if compat
                .get("requiresReasoningContentOnAssistantMessages")
                .and_then(Value::as_bool)
                == Some(true)
                && message.get("role").and_then(Value::as_str) == Some("assistant")
                && message.get("reasoning_content").is_none_or(Value::is_null)
            {
                message["reasoning_content"] = message
                    .get("reasoning")
                    .filter(|value| value.is_string())
                    .cloned()
                    .unwrap_or(json!(""));
            }
        }
    }
    let mut headers = endpoints::headers();
    headers.push(("Authorization".into(), format!("Bearer {token}")));
    headers.push(("Content-Type".into(), "application/json".into()));
    Ok(ChatPlan::chat(
        format!("{}/chat/completions", endpoints::MODEL_ORIGIN),
        headers,
        body,
    ))
}

pub fn classify(status: u16, body: &Value) -> proto::ErrorClass {
    let message = client::message(body);
    let lower = message.to_ascii_lowercase();
    let code = body
        .pointer("/error/code")
        .or_else(|| body.get("code"))
        .and_then(Value::as_i64);
    let kind = body
        .pointer("/error/code")
        .or_else(|| body.pointer("/error/type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if kind == "content_filter"
        || kind == "content_policy_violation"
        || lower.contains("content policy")
        || message.contains("内容安全")
        || message.contains("内容审核")
    {
        return proto::ErrorClass::ContentBlocked {
            status,
            message,
            upstream_code: code,
        };
    }
    if status == 401 || matches!(kind, "invalid_api_key" | "token_expired" | "invalid_token") {
        return proto::ErrorClass::TokenExpired { message };
    }
    let balance = matches!(kind, "insufficient_quota" | "insufficient_balance")
        || [
            "余额不足",
            "积分不足",
            "额度不足",
            "额度已用完",
            "insufficient quota",
            "insufficient balance",
            "quota exceeded",
        ]
        .iter()
        .any(|marker| lower.contains(marker));
    if balance || status == 429 || status == 402 {
        return proto::ErrorClass::QuotaLimited {
            reset_at: if status == 429 && !balance {
                Some(crate::http::now_ms() + 60_000)
            } else {
                None
            },
            message,
            upstream_code: code,
            status,
        };
    }
    proto::ErrorClass::Fatal {
        status,
        message,
        upstream_code: code,
    }
}

pub async fn reasoning_patch(
    ctx: &PluginCtx,
    level: &str,
    model: &str,
    body: &Value,
) -> proto::ReasoningPatch {
    let skip = || proto::ReasoningPatch::Skip {
        reason: "QuWork 按模型目录和客户端设置处理思考档位".into(),
    };
    if crate::reasoning::read_client_level(body).is_some()
        || body.get("thinking").is_some_and(|value| !value.is_null())
    {
        return skip();
    }
    let catalog = models::current(ctx).await;
    let level = if level == "none" { "off" } else { level };
    let Some(wire) = catalog
        .model(model)
        .and_then(|row| row.get("reasoningEfforts"))
        .and_then(|levels| levels.get(level))
        .and_then(Value::as_str)
    else {
        return skip();
    };
    proto::ReasoningPatch::Set {
        field: "reasoning_effort".into(),
        value: json!(wire),
    }
}
