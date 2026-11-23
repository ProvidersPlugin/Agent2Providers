//! Gemini v1internal 信封的本家薄适配，迁移原生 outbound.rs 并按职责拆分。
//! 不用通用 Gemini API /models 路径，requestType只在图片生成时出现。

use super::{catalog_shape, contents, reasoning, schema, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};
use std::collections::BTreeSet;

pub fn build(
    chat: &Value,
    model: &str,
    project: &str,
    enterprise: bool,
    entry: Option<&Value>,
) -> Result<Value> {
    if project.trim().is_empty() {
        return Err(Error::http(
            403,
            "Antigravity 尚未取得账号项目，请重新授权或填写已有项目 ID",
        )
        .with_code("local_precondition"));
    }
    let messages = chat["messages"]
        .as_array()
        .ok_or_else(|| Error::http(400, "Antigravity 请求缺少 messages 数组"))?;
    let image = model.to_ascii_lowercase().contains("image");
    let system = system(messages);
    let system_text = system
        .as_ref()
        .and_then(|s| s.pointer("/parts/0/text"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut contents = contents::contents(messages)?;
    if catalog_shape::claude(model) {
        contents::claude_leading(&mut contents);
    }
    let first_user = contents
        .iter()
        .find(|turn| turn["role"] == "user")
        .and_then(|turn| turn.pointer("/parts/0/text"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let session = session(project, model, system_text, first_user);
    let mut request = json!({ "generationConfig": reasoning::config(chat, model, entry, image),
        "sessionId": session });
    request["contents"] = Value::Array(contents);
    if !image {
        if let Some(system) = system {
            request["systemInstruction"] = system;
        }
        if let Some(tools) = tools(chat)? {
            request["tools"] = json!([{ "functionDeclarations": tools }]);
            request["toolConfig"] = json!({ "functionCallingConfig": { "mode": "VALIDATED" } });
        }
    }
    let mut envelope = json!({ "model": model, "userAgent": if enterprise { "jetski" } else { "antigravity" },
        "requestId": format!("agent/{}/{}", util::now()?, util::random_hex(4)?) });
    envelope["request"] = request;
    if !project.trim().is_empty() {
        envelope["project"] = json!(project.trim());
    }
    if image {
        envelope["requestType"] = json!("image_gen");
    }
    Ok(envelope)
}

fn system(messages: &[Value]) -> Option<Value> {
    let texts: Vec<String> = messages
        .iter()
        .filter(|message| matches!(contents::role(message).as_str(), "system" | "developer"))
        .map(|message| sanitize(&contents::text(&message["content"])))
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .collect();
    (!texts.is_empty())
        .then(|| json!({ "role": "user", "parts": [{ "text": texts.join("\n\n") }] }))
}
fn sanitize(text: &str) -> String {
    let text = text.replace(
        "You are a Claude agent, built on Anthropic's Claude Agent SDK.",
        "",
    );
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut index = 0;
    let mut last = 0;
    while index + 8 <= bytes.len() {
        if bytes[index..index + 8].eq_ignore_ascii_case(b"opencode") {
            output.push_str(&text[last..index]);
            output.push_str(match &text[index..index + 8] {
                "OpenCode" => "Antigravity",
                "OPENCODE" => "ANTIGRAVITY",
                _ => "antigravity",
            });
            index += 8;
            last = index;
        } else {
            index += 1;
        }
    }
    output.push_str(&text[last..]);
    output
}
fn tools(chat: &Value) -> Result<Option<Vec<Value>>> {
    if chat["tool_choice"]
        .as_str()
        .is_some_and(|s| s.trim().eq_ignore_ascii_case("none"))
        || chat["tool_choice"]["type"]
            .as_str()
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("none"))
    {
        return Ok(None);
    }
    let Some(tools) = chat.get("tools").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let tools = tools
        .as_array()
        .ok_or_else(|| Error::http(400, "Antigravity 工具必须是数组"))?;
    let mut seen = BTreeSet::new();
    let mut output = Vec::new();
    for tool in tools {
        if tool["type"].as_str().is_some_and(|kind| kind != "function") {
            return Err(Error::http(
                400,
                "Antigravity 仅支持函数工具，未丢弃工具声明",
            ));
        }
        let function = tool.get("function").unwrap_or(tool);
        let name = contents::field(function, "name").trim().to_string();
        if name.is_empty() || !seen.insert(name.clone()) {
            return Err(Error::http(400, "Antigravity 工具名称为空或重复"));
        }
        let mut declaration =
            json!({ "name": name, "parameters": schema::clean_schema(&function["parameters"]) });
        if let Some(description) = function
            .get("description")
            .filter(|v| !v.is_null() && v.as_str() != Some(""))
        {
            declaration["description"] = description.clone();
        }
        output.push(declaration);
    }
    Ok((!output.is_empty()).then_some(output))
}
fn session(project: &str, model: &str, system: &str, first_user: &str) -> String {
    let mut hash: i64 = -3750763034362895579;
    for byte in format!("{project}|{model}|{system}|{first_user}").bytes() {
        hash = hash.wrapping_mul(1099511628211);
        hash ^= i64::from(byte);
    }
    hash.to_string()
}
