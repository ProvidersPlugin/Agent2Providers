//! 与原生Kiro一致的预算前缀/目录声明自适应思考，JSON格式仅保留明确的软约束。
//! 没有服务端强制schema证据：strict=true明确拒绝，不假称提示词等于结构化协议。

use crate::models;
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn effort(body: &Value) -> &str {
    body["reasoning_effort"]
        .as_str()
        .or_else(|| body["reasoning"]["effort"].as_str())
        .unwrap_or("")
}
pub fn budget(body: &Value, model: &str) -> Result<Option<u32>> {
    let level = effort(body);
    let mode = body["thinking"]["type"].as_str().unwrap_or("");
    if !mode.is_empty() && !["enabled", "disabled", "adaptive"].contains(&mode) {
        return Err(Error::http(400, "Kiro 思考方式无效"));
    }
    if !level.is_empty()
        && !["none", "off", "low", "medium", "high", "xhigh", "max"].contains(&level)
    {
        return Err(Error::http(400, "Kiro 不支持此思考档位"));
    }
    let explicit = body
        .pointer("/thinking/budget_tokens")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_u64()
                .filter(|value| (1024..=50_000).contains(value))
                .map(|value| value as u32)
                .ok_or_else(|| Error::http(400, "Kiro 思考预算必须为1024至50000"))
        })
        .transpose()?;
    if mode == "disabled" || matches!(level, "none" | "off") {
        if explicit.is_some()
            || matches!(mode, "enabled" | "adaptive")
            || (!level.is_empty() && !matches!(level, "none" | "off"))
        {
            return Err(Error::http(400, "Kiro 思考设置互相冲突"));
        }
        return Ok(None);
    }
    let amount = explicit.or(match level {
        "low" => Some(10_000),
        "medium" => Some(20_000),
        "high" => Some(30_000),
        "xhigh" | "max" => Some(50_000),
        _ => None,
    });
    if amount.is_some() && !models::thinks(model) {
        return Err(Error::http(400, "当前Kiro模型未声明可调思考"));
    }
    if mode == "enabled" && amount.is_none() {
        return Err(Error::http(400, "请为Kiro思考设置预算或档位"));
    }
    if mode == "adaptive" && level.is_empty() {
        return Err(Error::http(400, "Kiro自适应思考需要目录允许的档位"));
    }
    Ok(amount)
}
pub fn output_instruction(body: &Value) -> Result<String> {
    let format = body
        .get("response_format")
        .or_else(|| body.get("output_format"))
        .filter(|value| !value.is_null());
    let Some(format) = format else {
        return Ok(String::new());
    };
    if format["strict"] == true || format["json_schema"]["strict"] == true {
        return Err(Error::http(
            400,
            "Kiro只有提示词式JSON约束，不能保证strict结构化输出",
        ));
    }
    match format["type"].as_str() {
        Some("text") => Ok(String::new()),
        Some("json_object") => Ok("Respond with a single JSON object and nothing else.".into()),
        Some("json_schema") => {
            let schema = format["json_schema"]
                .get("schema")
                .or_else(|| format.get("schema"))
                .filter(|value| value.is_object())
                .ok_or_else(|| Error::http(400, "Kiro JSON结构缺少有效schema"))?;
            Ok(format!("Respond with a single JSON value and nothing else, matching this JSON schema:\n{schema}."))
        }
        _ => Err(Error::http(400, "Kiro输出格式无效或未支持，未静默忽略")),
    }
}
pub fn patch(level: &str, body: &Value) -> Value {
    if !body.get("reasoning_effort").is_none_or(Value::is_null) {
        return json!({ "action": "skip", "reason": "客户端已指定思考档位" });
    }
    if ["low", "medium", "high", "xhigh", "max"].contains(&level) {
        json!({ "action": "set", "field": "reasoning_effort", "value": level })
    } else {
        json!({ "action": "skip", "reason": "Kiro不支持该思考档位" })
    }
}
