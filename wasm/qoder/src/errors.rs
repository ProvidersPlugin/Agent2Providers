//! 上游嵌套 JSON/字符串错误先读语义再读 HTTP。只向主机输出四档分类。
//! 10605 排队是 503/fatal（经声明映射为限流拥塞换号）并保留原始队列诊断字段；105 才是过期，裸 403 不自动续期。
use super::util;
use agent2api_wasm_sdk::{json, Error, Value};

const QUOTA: &[i64] = &[110, 112, 113, 114, 115, 116, 117, 118, 119, 122];
const REQUEST: &[i64] = &[406, 416, 430, 80411, 80412, 100400, 100401, 100403];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueueInfo {
    pub is_queued: Option<bool>,
    pub service_available: Option<bool>,
    pub queue_type: Option<String>,
    pub queue_count: Option<i64>,
    pub retry_after_seconds: Option<u64>,
    pub wait_time: Option<u64>,
    pub sub_code: Option<String>,
    pub model_key: Option<String>,
}

impl QueueInfo {
    pub fn present(&self) -> bool {
        self.is_queued.is_some()
            || self.service_available.is_some()
            || self.queue_type.is_some()
            || self.queue_count.is_some()
            || self.retry_after_seconds.is_some()
            || self.wait_time.is_some()
            || self.sub_code.is_some()
    }

    pub fn ready(&self) -> bool {
        self.is_queued == Some(false) && self.service_available != Some(false)
    }

    pub fn wait_seconds(&self) -> Option<u64> {
        self.retry_after_seconds.or(self.wait_time)
    }

    pub fn json(&self) -> Value {
        let mut map = serde_json::Map::new();
        if let Some(value) = self.is_queued {
            map.insert("isQueued".into(), json!(value));
        }
        if let Some(value) = self.service_available {
            map.insert("serviceAvailable".into(), json!(value));
        }
        if let Some(value) = &self.queue_type {
            map.insert("queueType".into(), json!(value));
        }
        if let Some(value) = self.queue_count {
            map.insert("queueCount".into(), json!(value));
        }
        if let Some(value) = self.retry_after_seconds {
            map.insert("retryAfterSeconds".into(), json!(value));
        }
        if let Some(value) = self.wait_time {
            map.insert("waitTime".into(), json!(value));
        }
        if let Some(value) = &self.sub_code {
            map.insert("subCode".into(), json!(value));
        }
        if let Some(value) = &self.model_key {
            map.insert("modelKey".into(), json!(value));
        }
        Value::Object(map)
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(kind) = &self.queue_type {
            parts.push(format!("队列 {kind}"));
        }
        if let Some(count) = self.queue_count {
            parts.push(format!("前方 {count}"));
        }
        if let Some(available) = self.service_available {
            parts.push(if available {
                "服务可用".into()
            } else {
                "服务不可用".into()
            });
        }
        if let Some(seconds) = self.wait_seconds() {
            parts.push(format!("建议 {seconds}s 后重试"));
        }
        if let Some(sub) = &self.sub_code {
            parts.push(format!("子码 {sub}"));
        }
        parts.join("，")
    }
}

#[derive(Default)]
struct Signals {
    codes: Vec<i64>,
    queued: bool,
    retry: Option<u64>,
    queue: QueueInfo,
    text: String,
    content: bool,
}

fn short_field(value: &Value, max: usize) -> Option<String> {
    let text = value.as_str()?.trim();
    if text.is_empty() || !util::printable(text, max) {
        return None;
    }
    Some(text.to_string())
}

fn scan(value: &Value, depth: usize, output: &mut Signals) {
    if depth > 10 {
        return;
    }
    match value {
        Value::Object(fields) => {
            for key in ["code", "statusCodeValue", "upstream_code", "upstreamCode"] {
                if let Some(code) = util::integer(&value[key]) {
                    output.codes.push(code);
                }
            }
            if output.queue.is_queued.is_none() {
                output.queue.is_queued = value.get("isQueued").and_then(Value::as_bool);
            }
            if output.queue.service_available.is_none() {
                output.queue.service_available =
                    value.get("serviceAvailable").and_then(Value::as_bool);
            }
            if output.queue.queue_type.is_none() {
                output.queue.queue_type = value
                    .get("queueType")
                    .and_then(|value| short_field(value, 32));
            }
            if output.queue.queue_count.is_none() {
                output.queue.queue_count = value.get("queueCount").and_then(util::integer);
            }
            if output.queue.retry_after_seconds.is_none() {
                output.queue.retry_after_seconds = value
                    .get("retryAfterSeconds")
                    .and_then(util::integer)
                    .and_then(|value| u64::try_from(value).ok());
            }
            if output.queue.wait_time.is_none() {
                output.queue.wait_time = value
                    .get("waitTime")
                    .and_then(util::integer)
                    .and_then(|value| u64::try_from(value).ok());
            }
            if output.queue.sub_code.is_none() {
                output.queue.sub_code = value
                    .get("subCode")
                    .and_then(|value| short_field(value, 64));
            }
            if output.queue.model_key.is_none() {
                output.queue.model_key = value
                    .get("modelKey")
                    .and_then(|value| short_field(value, 64));
            }
            output.queued |= value["isQueued"] == true
                || value["serviceAvailable"] == false && value["queueType"].is_string();
            if output.retry.is_none() {
                output.retry = ["retryAfterSeconds", "waitTime"].iter().find_map(|key| {
                    util::integer(&value[*key]).and_then(|value| u64::try_from(value).ok())
                });
            }
            output.content |=
                value["code"] == "content_filter" || value["type"] == "content_filter";
            for key in [
                "message", "body", "data", "result", "error", "msg", "details", "queue",
            ] {
                if let Some(child) = fields.get(key) {
                    scan(child, depth + 1, output);
                }
            }
            if fields.contains_key("pricingUrl") {
                output.text.push_str(" pricingurl ");
            }
        }
        Value::String(text) => {
            if output.text.len() < 16 * 1024 {
                output.text.extend(text.chars().take(4096));
                output.text.push(' ');
            }
            if text.trim_start().starts_with('{') {
                if let Ok(value) = serde_json::from_str::<Value>(text) {
                    scan(&value, depth + 1, output);
                }
            }
        }
        _ => {}
    }
}

pub fn queue_info(body: &Value) -> Option<QueueInfo> {
    let mut signals = Signals::default();
    scan(body, 0, &mut signals);
    signals.queue.present().then_some(signals.queue)
}

pub fn has_error(value: &Value) -> bool {
    envelope_error(value, 0)
}

fn envelope_error(value: &Value, depth: usize) -> bool {
    if depth > 10 {
        return false;
    }
    if let Some(text) = value
        .as_str()
        .filter(|text| text.len() <= 1024 * 1024 && text.trim_start().starts_with('{'))
    {
        return serde_json::from_str::<Value>(text)
            .ok()
            .is_some_and(|value| envelope_error(&value, depth + 1));
    }
    if !value.is_object() {
        return false;
    }
    if value.get("error").is_some_and(|value| !value.is_null())
        || value["success"] == false
        || value["type"] == "error"
        || value["isQueued"] == true
        || value["serviceAvailable"] == false && value["queueType"].is_string()
    {
        return true;
    }
    if ["statusCodeValue", "code"].iter().any(|key| {
        let Some(code) = value
            .get(*key)
            .filter(|value| !value.is_null() && *value != "")
        else {
            return false;
        };
        util::integer(code)
            .map(|code| code != 0 && code != 200)
            .unwrap_or_else(|| !matches!(code.as_str(), Some("SUCCESS" | "OK" | "success")))
    }) {
        return true;
    }
    value
        .get("body")
        .is_some_and(|child| envelope_error(child, depth + 1))
}

fn busy_message(queue: &QueueInfo, extra: Option<&str>) -> String {
    let detail = queue.summary();
    let mut message = if detail.is_empty() {
        "Qoder 模型正在排队，这不是登录态失效或账号额度不足，请稍后重试".to_string()
    } else {
        format!("Qoder 模型正在排队（{detail}），这不是登录态失效或账号额度不足，请稍后重试")
    };
    if let Some(note) = extra.filter(|note| !note.is_empty()) {
        message.push('（');
        message.push_str(note);
        message.push('）');
    }
    message
}

pub fn queued_failure(queue: &QueueInfo, extra: Option<&str>) -> Error {
    let mut error = Error::http(503, busy_message(queue, extra)).with_code("qoder_model_busy");
    error.upstream_code = Some(10605);
    error.reset_at = queue
        .wait_seconds()
        .and_then(|seconds| seconds.checked_mul(1000))
        .and_then(|millis| i64::try_from(millis).ok())
        .and_then(|millis| util::now().ok()?.checked_add(millis));
    error
}

pub fn classify(status: u16, body: &Value) -> Value {
    let mut signals = Signals::default();
    scan(body, 0, &mut signals);
    let text = signals.text.to_ascii_lowercase();
    let upstream = signals
        .codes
        .iter()
        .copied()
        .find(|code| {
            *code == 10605 || *code == 105 || QUOTA.contains(code) || REQUEST.contains(code)
        })
        .or_else(|| {
            signals
                .codes
                .iter()
                .copied()
                .find(|code| *code != 200 && *code != 0)
        });
    let (kind, http, code, message) =
        if signals.queued
            || signals.codes.contains(&10605)
            || body["code"] == "qoder_model_busy"
            || body["providerCode"] == "qoder_model_busy"
        {
            let existing = body["message"]
                .as_str()
                .map(str::trim)
                .filter(|msg| {
                    !signals.queue.present()
                        && msg.starts_with("Qoder 模型正在排队")
                        && msg.len() <= 512
                        && !msg.chars().any(char::is_control)
                })
                .map(str::to_string);
            (
                "fatal",
                503,
                "qoder_model_busy",
                existing.unwrap_or_else(|| busy_message(&signals.queue, None)),
            )
        } else if signals.codes.contains(&105) || body["code"] == "qoder_auth_expired" {
            (
                "token_expired",
                401,
                "qoder_auth_expired",
                "Qoder 登录态已失效，请续期或重新授权".into(),
            )
        } else if text.contains("error in upstream response") {
            (
                "fatal",
                502,
                "qoder_upstream_error",
                "Qoder 后端模型返回错误，当前账号登录态未被判为失效".into(),
            )
        } else if signals.codes.iter().any(|code| REQUEST.contains(code)) {
            (
                "fatal",
                400,
                "qoder_request_rejected",
                "Qoder 拒绝当前请求，请核对模型支持的输入、图片数量和上下文长度".into(),
            )
        } else if signals.content
            || status >= 400
                && [
                    "blocked by security policy",
                    "unapproved channel",
                    "illegal api invocation",
                ]
                .iter()
                .any(|word| text.contains(*word))
        {
            (
                "content_blocked",
                if status >= 400 { status } else { 400 },
                "content_filter",
                "Qoder 内容策略拒绝了当前请求，不是账号额度问题".into(),
            )
        } else if signals.codes.iter().any(|code| QUOTA.contains(code))
            || body["code"] == "qoder_quota_limited"
            || [
                "pricingurl",
                "insufficient",
                "no_quota",
                "quota_exceed",
                "exceed_quota",
                "exceeded",
                "credit",
                "upgrade",
                "subscription",
                "plan",
                "trial",
            ]
            .iter()
            .any(|word| text.contains(*word))
        {
            (
                "quota_limited",
                429,
                "qoder_quota_limited",
                "Qoder 当前账号额度不足或套餐不支持该模型".into(),
            )
        } else if status == 429
            || body["code"] == "qoder_rate_limited"
            || text.contains("rate limit")
            || text.contains("too many")
        {
            (
                "quota_limited",
                429,
                "qoder_rate_limited",
                "Qoder 请求过于频繁，请稍后重试".into(),
            )
        } else if status == 401
            && signals
                .codes
                .iter()
                .all(|code| matches!(*code, 0 | 200 | 401))
        {
            (
                "token_expired",
                401,
                "qoder_auth_expired",
                "Qoder 登录态已失效，请续期或重新授权".into(),
            )
        } else if status == 403 {
            (
                "fatal",
                403,
                "qoder_forbidden",
                "Qoder 拒绝访问，请确认当前账号的地区与模型权限".into(),
            )
        } else {
            (
                "fatal",
                if (400..600).contains(&status) {
                    status
                } else {
                    502
                },
                "qoder_upstream_error",
                "Qoder 上游请求未完成或返回了无效结果".into(),
            )
        };
    let reset_at = signals
        .retry
        .filter(|_| code == "qoder_model_busy" || code == "qoder_rate_limited")
        .and_then(|seconds| seconds.checked_mul(1000))
        .and_then(|millis| i64::try_from(millis).ok())
        .and_then(|millis| util::now().ok()?.checked_add(millis))
        .or_else(|| {
            if code == "qoder_model_busy" || code == "qoder_rate_limited" {
                util::integer(&body["resetAt"]).or_else(|| util::integer(&body["reset_at"]))
            } else {
                None
            }
        });
    let mut result = json!({
        "kind": kind,
        "status": http,
        "code": code,
        "message": message,
        "upstream_code": upstream,
        "reset_at": reset_at,
        "retry_after_ms": signals.retry.map(|seconds| seconds.clamp(5, 30) * 1000),
    });
    if signals.queue.present() {
        result["queue"] = signals.queue.json();
    }
    result
}

pub fn failure(status: u16, body: &Value) -> Error {
    let class = classify(status, body);
    let mut error = Error::http(
        class["status"].as_u64().unwrap_or(502) as u16,
        class["message"].as_str().unwrap_or("Qoder 请求失败"),
    )
    .with_code(class["code"].as_str().unwrap_or("qoder_upstream_error"));
    error.upstream_code = class["upstream_code"].as_i64();
    error.reset_at = class["reset_at"].as_i64();
    error
}

pub fn event(error: &Error) -> Value {
    json!({ "error": { "message": error.message, "status": error.status.unwrap_or(502), "code": error.code,
        "upstreamCode": error.upstream_code, "resetAt": error.reset_at,
        "type": if error.status == Some(429) { "rate_limit_error" }
            else if error.status == Some(401) { "authentication_error" } else { "upstream_error" } } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_nested_10605_queue_diagnostics() {
        let envelope = json!({
            "statusCodeValue": 403,
            "body": "{\"code\":\"403\",\"message\":\"{\\\"code\\\":\\\"10605\\\",\\\"message\\\":\\\"{\\\\\\\"errorCode\\\\\\\":0,\\\\\\\"isQueued\\\\\\\":true,\\\\\\\"modelKey\\\\\\\":\\\\\\\"qfmodel\\\\\\\",\\\\\\\"queueCount\\\\\\\":1,\\\\\\\"queueType\\\\\\\":\\\\\\\"p4\\\\\\\",\\\\\\\"retryAfterSeconds\\\\\\\":60,\\\\\\\"serviceAvailable\\\\\\\":true,\\\\\\\"subCode\\\\\\\":\\\\\\\"RESOURCE_LIMIT\\\\\\\",\\\\\\\"waitTime\\\\\\\":60}\\\",\\\"subCode\\\":\\\"RESOURCE_LIMIT\\\"}\"}"
        });
        let info = queue_info(&envelope).expect("queue info");
        assert_eq!(info.is_queued, Some(true));
        assert_eq!(info.service_available, Some(true));
        assert_eq!(info.queue_type.as_deref(), Some("p4"));
        assert_eq!(info.queue_count, Some(1));
        assert_eq!(info.retry_after_seconds, Some(60));
        assert_eq!(info.sub_code.as_deref(), Some("RESOURCE_LIMIT"));
        assert!(!info.ready());
        assert!(info.summary().contains("队列 p4"));
        assert!(info.summary().contains("前方 1"));
        assert!(info.summary().contains("子码 RESOURCE_LIMIT"));
    }

    #[test]
    fn reclassify_preserves_formatted_queue_message_and_reset_at() {
        let detail = json!({
            "code": 10605,
            "providerCode": "qoder_model_busy",
            "upstreamCode": 10605,
            "message": "Qoder 模型正在排队（队列 p4，前方 1，服务可用，建议 60s 后重试，子码 RESOURCE_LIMIT），这不是登录态失效或账号额度不足，请稍后重试",
            "resetAt": 1795847073079_i64
        });
        let result = classify(503, &detail);
        assert_eq!(result["code"], "qoder_model_busy");
        assert_eq!(result["status"], 503);
        assert_eq!(result["reset_at"], 1795847073079_i64);
        assert!(result["message"].as_str().unwrap_or("").contains("队列 p4"));
        assert!(result["message"].as_str().unwrap_or("").contains("子码 RESOURCE_LIMIT"));
    }

    #[test]
    fn queue_ready_requires_not_queued_and_service_available() {
        let not_ready = QueueInfo {
            is_queued: Some(false),
            service_available: Some(false),
            ..Default::default()
        };
        assert!(!not_ready.ready());
        let ready = QueueInfo {
            is_queued: Some(false),
            service_available: Some(true),
            ..Default::default()
        };
        assert!(ready.ready());
        let ready_when_omitted = QueueInfo {
            is_queued: Some(false),
            service_available: None,
            ..Default::default()
        };
        assert!(ready_when_omitted.ready());
    }
}
