//! 通用Chat→Responses转换交给宿主；只保留AStudio原生请求白名单与别名参数适配。

use agent2api_wasm_sdk::{json, protocol, Error, Result, Value};

pub fn model(params: &Value) -> Result<String> {
    let model = params["wire_model"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            params["body"]["model"]
                .as_str()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or("astronclaw-auto");
    if model.len() > 512 || model.chars().any(char::is_control) {
        return Err(Error::http(400, "AStudio 模型名称格式无效"));
    }
    Ok(model.to_string())
}

pub fn build(body: &Value, model: &str) -> Result<Value> {
    if !body.is_object() {
        return Err(Error::http(400, "AStudio 请求体必须是JSON对象"));
    }
    let mut original = body.clone();
    original["stream"] = json!(true);
    if !original["reasoning_effort"].is_string() {
        if let Some(effort) = body.pointer("/reasoning/effort").and_then(Value::as_str) {
            original["reasoning_effort"] = json!(effort);
        }
    }
    // 原生兼容扁平function声明；通用转换器以标准Chat嵌套形态为输入。
    if let Some(tools) = original.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if tool.is_object()
                && tool["name"].is_string()
                && tool["function"].is_null()
                && tool.get("type").is_none_or(|value| *value == "function")
            {
                *tool = json!({ "type": "function", "function": tool.clone() });
            }
        }
    }
    let mut converted = protocol::request("responses", &original, model)?;
    if !converted.is_object() {
        return Err(Error::http(502, "AStudio 请求协议转换结果无效"));
    }
    // 通用转换器已筛选合法 Responses 字段；不能二次删掉 service_tier/user/metadata 等。
    if let Some(map) = converted.as_object_mut() {
        map.retain(|key, _| !key.starts_with("_wb_"));
    }
    if let Some(value) = body
        .get("max_output_tokens")
        .filter(|value| !value.is_null())
    {
        converted["max_output_tokens"] = value.clone();
    }
    // 保持原生“不主动请求none档”的行为；其它档位不降级、不强制改high。
    if original["reasoning_effort"] == "none" {
        if let Some(map) = converted.as_object_mut() {
            map.remove("reasoning");
        }
    } else if let Some(reasoning) = converted
        .get_mut("reasoning")
        .and_then(Value::as_object_mut)
    {
        reasoning.remove("summary");
    }
    if let Some(tools) = converted.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if tool["type"] == "function" && tool["strict"].is_null() {
                tool["strict"] = json!(false);
            }
        }
    }
    converted["stream"] = json!(true);
    Ok(converted)
}

pub fn reasoning(params: &Value) -> Value {
    if params["body"]
        .get("reasoning_effort")
        .is_some_and(|value| !value.is_null())
        || params["body"]
            .pointer("/reasoning/effort")
            .is_some_and(|value| !value.is_null())
    {
        return json!({ "action": "skip", "reason": "客户端已指定思考档位" });
    }
    let level = params["level"].as_str().unwrap_or("");
    if matches!(level, "low" | "medium" | "high" | "xhigh" | "max") {
        json!({ "action": "set", "field": "reasoning_effort", "value": level })
    } else {
        json!({ "action": "skip", "reason": "AStudio 不支持该思考档位" })
    }
}
