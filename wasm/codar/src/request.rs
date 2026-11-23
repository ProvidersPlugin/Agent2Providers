use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

use crate::{models, reasoning, util};

pub fn build(params: &Value, model: &Value) -> Result<(String, Value)> {
    let id = model["id"]
        .as_str()
        .ok_or_else(|| Error::http(400, "请指定科搭模型"))?;
    let protocol = models::protocol(model)?;
    let source = &params["body"];
    if !source.is_object() || source["messages"].as_array().is_none_or(Vec::is_empty) {
        return Err(Error::http(400, "科搭对话至少需要一条消息"));
    }
    if model["supportsImages"] == false
        && source["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|message| message["content"].as_array().into_iter().flatten())
            .any(|part| {
                matches!(
                    part["type"].as_str(),
                    Some("image_url" | "image" | "input_image")
                )
            })
    {
        return Err(Error::http(
            400,
            "科搭当前模型目录声明不支持图片输入，请选择支持图片的模型",
        ));
    }
    let mut body = if protocol == "chat" {
        source.clone()
    } else {
        sdk::protocol::request(protocol, source, id)?
    };
    if protocol == "anthropic" {
        // 通用 Chat→Anthropic 转换只保留标准参数；科搭目录里的原生思考字段需显式保留。
        for field in ["thinking", "output_config"] {
            if let Some(value) = source.get(field) {
                body[field] = value.clone();
            }
        }
    }
    body["model"] = json!(id);
    body["stream"] = json!(true);
    if protocol == "chat" {
        if body["stream_options"].is_null() {
            body["stream_options"] = json!({});
        }
        if let Some(options) = body["stream_options"].as_object_mut() {
            options.insert("include_usage".into(), json!(true));
        }
    }
    let field = if protocol == "responses" {
        "max_output_tokens"
    } else if protocol == "chat" && body.get("max_completion_tokens").is_some() {
        "max_completion_tokens"
    } else {
        "max_tokens"
    };
    if body.get(field).is_none() {
        if let Some(limit) = model["maxOutputTokens"].as_u64() {
            body[field] = json!(limit.min(8192));
        } else if protocol == "anthropic" {
            body[field] = json!(8192);
        }
    }
    if let Some(maximum) = model["maxOutputTokens"].as_u64() {
        if body[field].as_u64().is_some_and(|asked| asked > maximum) {
            body[field] = json!(maximum);
        }
    }
    let level = util::text(params, &["reasoning"]);
    let level = if level.is_empty() {
        util::text(source, &["reasoning_effort"])
    } else {
        level
    };
    let level = if level.is_empty() {
        source["output_config"]["effort"]
            .as_str()
            .unwrap_or("")
            .to_string()
    } else {
        level
    };
    let level = if level.is_empty()
        && source.get("thinking").is_none()
        && source.get("output_config").is_none()
    {
        model["reasoningEffortDefault"]
            .as_str()
            .unwrap_or("")
            .to_string()
    } else {
        level
    };
    let rules = model["reasoningRules"]["levels"].as_array();
    let level = if level == "none"
        && rules.is_some_and(|rules| rules.iter().any(|rule| rule["id"] == "off"))
    {
        "off".to_string()
    } else {
        level
    };
    if !level.is_empty() && rules.is_some_and(|rules| !rules.is_empty()) {
        reasoning::apply(&mut body, model, &level)?;
    }
    Ok((protocol.to_string(), body))
}

pub fn reasoning(params: &Value) -> Value {
    let level = util::text(params, &["level", "reasoning"]);
    if level.is_empty() {
        return json!({ "action": "skip", "reason": "没有指定思考档位" });
    }
    json!({ "action": "set", "field": "reasoning_effort", "value": level })
}
