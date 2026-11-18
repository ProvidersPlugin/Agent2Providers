//! ADK → 核心 Gemini 的薄适配：只重装 candidates/content、字段名、工具片段与真实结束信号。
//! 不能把 content.parts/args_json 直接送进核心（它会丢帧或将每片当成一个工具）。

use crate::{credentials::Credentials, errors, tools::Tools};
use agent2api_wasm_sdk::{json, Error, Result, Value};

#[derive(Default)]
pub struct Adk {
    tools: Tools,
    usage: Option<Value>,
    finish: Option<String>,
    signature: Option<String>,
    pub output: bool,
    pub terminal: bool,
    text_bytes: usize,
}

pub fn field<'a>(value: &'a Value, camel: &str, snake: &str) -> Option<&'a Value> {
    value
        .get(camel)
        .filter(|value| !value.is_null())
        .or_else(|| value.get(snake).filter(|value| !value.is_null()))
}

fn text(value: &Value, camel: &str, snake: &str) -> String {
    field(value, camel, snake)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

pub fn failure(value: &Value, credential: &Credentials) -> Option<Error> {
    let code = field(value, "errorCode", "error_code")
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| value.to_string())
        })
        .unwrap_or_default();
    let message = text(value, "errorMessage", "error_message");
    let business = errors::code(value);
    let error = value.get("error").filter(|error| {
        !matches!(error, Value::Null | Value::Bool(false))
            && error.as_str().is_none_or(|text| !text.is_empty())
    });
    let explicit_failure = !code.is_empty() && !matches!(code.as_str(), "0" | "200");
    if explicit_failure
        || !message.is_empty() && !matches!(code.as_str(), "0" | "200")
        || !business.is_empty() && !matches!(business.as_str(), "0" | "200")
        || value["success"] == false
        || error.is_some()
    {
        let code = if !code.is_empty() { code } else { business };
        let message = if !message.is_empty() {
            message
        } else {
            errors::message(value)
        };
        return Some(errors::upstream(
            200,
            &code,
            &message,
            &credential.secrets(),
        ));
    }
    None
}

fn finish_reason(reason: &str) -> &'static str {
    match reason.trim().to_ascii_lowercase().as_str() {
        "max_tokens" | "max_output_tokens" | "length" => "MAX_TOKENS",
        "content_filter" | "safety" | "blocked" | "recitation" | "blocklist"
        | "prohibited_content" => "SAFETY",
        _ => "STOP",
    }
}

fn usage(value: &Value) -> Result<Value> {
    let mut converted = json!({});
    for (camel, snake) in [
        ("promptTokenCount", "prompt_token_count"),
        ("candidatesTokenCount", "candidates_token_count"),
        ("thoughtsTokenCount", "thoughts_token_count"),
        ("totalTokenCount", "total_token_count"),
        ("cachedContentTokenCount", "cached_content_token_count"),
    ] {
        if let Some(value) = field(value, camel, snake) {
            let number = value
                .as_i64()
                .or_else(|| {
                    value
                        .as_f64()
                        .filter(|value| value.is_finite())
                        .map(|value| value as i64)
                })
                .filter(|value| (0..=1_000_000_000_000).contains(value))
                .ok_or_else(|| Error::http(502, "Accio 用量字段无效"))?;
            converted[camel] = json!(number);
        }
    }
    Ok(converted)
}

impl Adk {
    pub fn consume(&mut self, value: &Value, credential: &Credentials) -> Result<Vec<Value>> {
        if let Some(error) = failure(value, credential) {
            return Err(error);
        }
        let raw_finish = text(value, "finishReason", "finish_reason");
        if !raw_finish.trim().is_empty() {
            self.finish = Some(finish_reason(&raw_finish).into());
        }
        if let Some(raw) = field(value, "usageMetadata", "usage_metadata") {
            if !raw.is_object() {
                return Err(Error::http(502, "Accio 用量不是有效对象"));
            }
            let read = usage(raw)?;
            if !read.as_object().is_some_and(|fields| fields.is_empty()) {
                let previous = self.usage.get_or_insert_with(|| json!({}));
                for (key, value) in read.as_object().into_iter().flatten() {
                    if previous[key]
                        .as_i64()
                        .is_none_or(|old| value.as_i64().is_some_and(|new| new > old))
                    {
                        previous[key] = value.clone();
                    }
                }
            }
        }
        let mut events = Vec::new();
        for part in value["content"]["parts"].as_array().into_iter().flatten() {
            let signature =
                field(part, "thoughtSignature", "thought_signature").and_then(Value::as_str);
            if let Some(signature) = signature.filter(|signature| !signature.is_empty()) {
                if signature.len() > 64 * 1024 {
                    return Err(Error::http(502, "Accio 思考签名超过限制"));
                }
                self.signature = Some(signature.into());
            }
            if let Some(call) = field(part, "functionCall", "function_call") {
                self.tools.accept(call, self.signature.as_deref())?;
                self.signature = None;
                continue;
            }
            if let Some(text) = part["text"].as_str().filter(|text| !text.is_empty()) {
                self.text_bytes = self.text_bytes.saturating_add(text.len());
                if self.text_bytes > 16 * 1024 * 1024 {
                    return Err(Error::http(502, "Accio 响应文本超过限制"));
                }
                if !text.trim().is_empty() {
                    self.output = true;
                }
                let mut normalized = json!({ "text": text });
                if part["thought"] == true {
                    normalized["thought"] = json!(true);
                }
                // 签名只随已匹配的完整工具发送。工具延后归并，文本先透出的签名不能
                // 留在核心 pendingSignature 中误配给另一条工具调用。
                events.push(json!({ "candidates": [{ "index": 0, "content": { "role": "model", "parts": [normalized] } }] }));
            }
        }
        // 实际 ADK 也会把具名工具首帧放在顶层，而参数在后续 content.parts。
        if let Some(call) = field(value, "functionCall", "function_call") {
            let signature = field(value, "thoughtSignature", "thought_signature")
                .and_then(Value::as_str)
                .or(self.signature.as_deref());
            if signature.is_some_and(|signature| signature.len() > 64 * 1024) {
                return Err(Error::http(502, "Accio 思考签名超过限制"));
            }
            self.tools.accept(call, signature)?;
            self.signature = None;
        }
        if field(value, "turnComplete", "turn_complete").and_then(Value::as_bool) == Some(true) {
            self.terminal = true;
        }
        Ok(events)
    }

    pub fn has_named_tool(&self) -> bool {
        self.tools.has_named()
    }

    pub fn has_usage(&self) -> bool {
        self.usage.is_some()
    }

    pub fn validate_end(&self) -> Result<()> {
        if !self.terminal && self.finish.is_none() {
            return Err(Error::http(502, "Accio 响应在上游完成前结束，未伪造成功"));
        }
        if !self.output && !self.tools.has_named() && self.finish.as_deref() != Some("SAFETY") {
            return Err(
                Error::http(502, "Accio 上游没有返回有效正文、思考或工具调用")
                    .with_code("accio_empty_response"),
            );
        }
        Ok(())
    }

    pub fn end(&mut self) -> Result<Vec<Value>> {
        self.validate_end()?;
        self.terminal = true;
        let mut events = Vec::new();
        for part in self.tools.finish()? {
            self.output = true;
            events.push(json!({ "candidates": [{ "index": 0, "content": { "role": "model", "parts": [part] } }] }));
        }
        let reason = self.finish.as_deref().unwrap_or("STOP");
        if !self.output && reason != "SAFETY" {
            return Err(
                Error::http(502, "Accio 上游没有返回有效正文、思考或工具调用")
                    .with_code("accio_empty_response"),
            );
        }
        let mut last = json!({ "candidates": [{ "index": 0, "finishReason": reason }] });
        if let Some(usage) = &self.usage {
            last["usageMetadata"] = usage.clone();
        }
        events.push(last);
        Ok(events)
    }
}

/// 正常对话外观的客户端版本拒绝也不是模型回答。只检查官方已知的完整提示。
pub fn version_notice(value: &Value) -> bool {
    value["content"]["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .any(|text| {
            text.trim_start()
                .to_ascii_lowercase()
                .starts_with("your app version is no longer supported")
        })
}
