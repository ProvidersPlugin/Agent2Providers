//! 沿用原生六档到 low/medium/high 的映射，客户端显式指定时不覆盖。

use agent2api_wasm_sdk::{json, Value};

fn normalize(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty() && value.chars().count() <= 32).then_some(value)
}

fn client_level(body: &Value) -> Option<&str> {
    for key in [
        "reasoning_effort",
        "reasoningEffort",
        "effort",
        "reasoning",
        "thinking",
    ] {
        match body.get(key) {
            Some(Value::String(value)) => return normalize(value),
            Some(Value::Object(value)) if key == "reasoning" => {
                if let Some(value) = value
                    .get("effort")
                    .and_then(Value::as_str)
                    .and_then(normalize)
                {
                    return Some(value);
                }
            }
            _ => {}
        }
    }
    None
}

fn explicit_off(body: &Value) -> bool {
    body["enable_thinking"] == false
        || body["chat_template_kwargs"]["enable_thinking"] == false
        || body["thinking"] == false
        || body["thinking"]["type"] == "disabled"
}

pub fn patch(level: &str, body: &Value) -> Value {
    let level = level.trim().to_ascii_lowercase();
    let skip = |reason: &str| json!({ "action": "skip", "reason": reason });
    if matches!(level.as_str(), "off" | "none") {
        return skip("Loomy 没有关闭思考的可靠表达，跳过注入");
    }
    if explicit_off(body) || client_level(body).is_some() {
        return skip("客户端请求体已指定思考设置，绑定让位");
    }
    let effort = match level.as_str() {
        "minimal" | "low" => "low",
        "medium" | "high" => "medium",
        "xhigh" | "max" => "high",
        _ => return skip("该等级不在通用档位表内，Loomy 不注入未知档位"),
    };
    json!({ "action": "set", "field": "reasoning_effort", "value": effort })
}
