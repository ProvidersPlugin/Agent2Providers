//! 仅移植原生已确认的 GLM-5.2/5.3 档位；5.3 套餐池单独留足思考与正文预算。
use agent2api_wasm_sdk::{json, Value};

fn family(model: &str, needle: &str) -> bool {
    let lower = model.trim().to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lower.get(from..).and_then(|rest| rest.find(needle)) {
        let end = from + at + needle.len();
        if !lower
            .as_bytes()
            .get(end)
            .is_some_and(|byte| byte.is_ascii_digit())
        {
            return true;
        }
        from = end;
    }
    false
}

fn level_53(value: &str) -> Option<&'static str> {
    let value = value.trim().to_ascii_lowercase();
    Some(match value.as_str() {
        "" => return None,
        "none" | "minimal" | "light" | "low" => "low",
        "medium" | "high" => "high",
        _ => "max",
    })
}

fn level_52(value: &str) -> Option<&'static str> {
    let value = value.trim().to_ascii_lowercase();
    Some(match value.as_str() {
        "" => return None,
        "off" | "none" | "minimal" => "minimal",
        "light" | "low" | "medium" | "high" => "high",
        _ => "max",
    })
}

fn maximum(body: &Value) -> Option<i64> {
    ["max_tokens", "max_completion_tokens"]
        .iter()
        .find_map(|key| body[*key].as_i64().filter(|n| *n > 0))
}

pub fn chat(body: &mut Value, model: &str) {
    if body["thinking"]["type"]
        .as_str()
        .is_some_and(|kind| kind.eq_ignore_ascii_case("disabled"))
    {
        return;
    }
    let effort = body["reasoning_effort"].as_str().unwrap_or("");
    let level = if family(model, "glm-5.3") {
        level_53(effort).or_else(|| maximum(body).filter(|value| *value < 1024).map(|_| "low"))
    } else if family(model, "glm-5.2") {
        level_52(effort)
    } else {
        None
    };
    if let Some(level) = level {
        body["reasoning_effort"] = json!(level);
    }
}

pub fn anthropic(payload: &mut Value, original: &Value, model: &str, selected: Option<&Value>) {
    let disabled = original["thinking"]["type"]
        .as_str()
        .is_some_and(|kind| kind.eq_ignore_ascii_case("disabled"));
    if disabled {
        payload["thinking"] = json!({ "type": "disabled" });
        if let Some(object) = payload.as_object_mut() {
            object.remove("output_config");
        }
        return;
    }
    if !family(model, "glm-5.3") {
        return;
    }
    let explicit = level_53(original["reasoning_effort"].as_str().unwrap_or(""));
    let short = maximum(original).is_some_and(|value| value < 1024);
    let level = explicit.unwrap_or(if short { "low" } else { "max" });
    let budget = if explicit.is_none() && short {
        1024
    } else {
        match level {
            "low" => 8_000,
            "high" => 16_000,
            _ => 32_000,
        }
    };
    let budget = original["thinking"]["budget_tokens"]
        .as_i64()
        .filter(|value| *value >= 1024)
        .unwrap_or(budget);
    let base = maximum(original)
        .or_else(|| payload["max_tokens"].as_i64())
        .filter(|value| *value > 0)
        .unwrap_or(1024);
    let limit = selected
        .and_then(|model| model["maxOutputTokens"].as_i64())
        .filter(|value| *value > 1024)
        .or_else(|| super::models::max_output(model))
        .unwrap_or(128_000);
    let budget = budget.min(limit - 1);
    let total = base.saturating_add(budget).min(limit).max(budget + 1);
    payload["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
    payload["output_config"] = json!({ "effort": level });
    payload["max_tokens"] = json!(total);
    if let Some(object) = payload.as_object_mut() {
        for key in ["temperature", "top_p", "top_k"] {
            object.remove(key);
        }
    }
}

pub fn patch(params: &Value) -> Value {
    let body = &params["body"];
    let skip = |reason| json!({ "action": "skip", "reason": reason });
    if body.get("reasoning_effort").is_some()
        || body["thinking"]["type"]
            .as_str()
            .is_some_and(|value| value.eq_ignore_ascii_case("disabled"))
    {
        return skip("客户端已指定思考参数，绑定不覆盖");
    }
    let model = params["model"].as_str().unwrap_or("");
    let level = params["level"].as_str().unwrap_or("").trim();
    if !["minimal", "low", "medium", "high", "xhigh", "max"]
        .iter()
        .any(|known| known.eq_ignore_ascii_case(level))
    {
        return skip("此档位没有可安全注入的目标值");
    }
    let target = if family(model, "glm-5.3") {
        level_53(level)
    } else if family(model, "glm-5.2") {
        level_52(level)
    } else {
        None
    };
    match target {
        Some(level) => json!({ "action": "set", "field": "reasoning_effort", "value": level }),
        None => skip("只有 GLM-5.2 / 5.3 家族具有已确认的思考档位"),
    }
}
