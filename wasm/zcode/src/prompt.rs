//! Start Plan 专属三段提示与四个缓存断点，模板逐字复用当前原生资源。
//! 本机路径、git、系统及 shell 只使用固定中性值；不引入 WASI/环境读取。
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use super::request_context::Context;

fn data() -> Result<Value> {
    serde_json::from_str(include_str!("zcode_system.json")).map_err(|_| Error::new("ZCode 提示模板资源不可用"))
}
fn text(value: &Value, key: &str) -> String { value[key].as_str().unwrap_or("").to_string() }
fn stable(data: &Value) -> String {
    data["stableSections"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join("\n\n")
}
fn dynamic(data: &Value, skill: bool) -> String {
    let env = &data["environment"];
    let environment = [text(env, "heading"), text(env, "invokedLine"),
        format!(" - {}: {{cwd}}", text(env, "cwdLabel")), format!(" - {}: {{git}}", text(env, "gitLabel")),
        format!(" - {}: {{platform}}", text(env, "platformLabel")), format!(" - {}: {{shell}}", text(env, "shellLabel")),
        format!(" - {}: {{os_version}}", text(env, "osVersionLabel")), format!(" - {}", text(env, "untrustedNote"))].join("\n");
    [text(data, "dynamicBehavior"), if skill { text(data, "sessionGuidance") } else { String::new() },
        environment, text(data, "poweredBy"), text(data, "contextManagement")].into_iter()
        .filter(|text| !text.is_empty()).collect::<Vec<_>>().join("\n\n")
}
fn substitute(value: &str, model: &str) -> String {
    let mut output = value.to_string();
    for (key, value) in [("{cwd}", "."), ("{git}", "false"), ("{platform}", "win32"),
        ("{shell}", "unknown"), ("{os_version}", "win32 x64"), ("{provider}", "bigmodel-api"), ("{model}", model)] {
        output = output.replace(key, value);
    }
    output
}
fn blocks(value: Option<&Value>) -> Vec<Value> {
    match value {
        Some(Value::String(text)) if !text.trim().is_empty() => vec![json!({ "type": "text", "text": text })],
        Some(Value::Array(items)) => items.iter().filter_map(|item| {
            let text = item.as_str().or_else(|| (item["type"] == "text").then(|| item["text"].as_str()).flatten())?;
            (!text.trim().is_empty()).then(|| json!({ "type": "text", "text": text }))
        }).collect(),
        _ => Vec::new(),
    }
}

pub fn apply(payload: &mut Value, model: &str, context: &Context) -> Result<()> {
    let data = data()?;
    let enabled = sdk::settings::get("gatewayPrompt")?.as_bool().unwrap_or(true);
    let overrides = sdk::settings::get("gatewayPromptText")?;
    let custom = blocks(payload.get("system"));
    if enabled {
        let skill = payload["tools"].as_array().is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "Skill"));
        let mut texts = [text(&data, "cliPrefix"), stable(&data), substitute(&dynamic(&data, skill), model)];
        for (at, field) in ["identity", "stable", "dynamic"].iter().enumerate() {
            if let Some(override_) = overrides[*field].as_str().filter(|text| !text.trim().is_empty()) {
                texts[at] = substitute(override_, model);
            }
        }
        let custom = custom.iter().filter_map(|block| block["text"].as_str()).collect::<Vec<_>>().join("\n\n");
        if !custom.is_empty() { texts[2].push_str("\n\n"); texts[2].push_str(&custom); }
        texts[2] = format!("\n\n{}", texts[2]);
        payload["system"] = json!(texts.iter().map(|text| json!({ "type": "text", "text": text,
            "cache_control": { "type": "ephemeral" } })).collect::<Vec<_>>());
        if let Some(messages) = payload["messages"].as_array_mut().filter(|messages| !messages.is_empty()) {
            let now = chrono::DateTime::from_timestamp_millis(sdk::now()).ok_or_else(|| Error::new("宿主时钟无效"))?;
            let zone = chrono::FixedOffset::east_opt(8 * 3600).ok_or_else(|| Error::new("时区无效"))?;
            let date = now.with_timezone(&zone).format("%Y-%m-%d").to_string();
            let text = text(&data["context"], "currentDateLine").replace("{date}", &date);
            messages.insert(0, json!({ "role": "user", "content": [{ "type": "text", "text": format!("<system-reminder>{text}</system-reminder>") }] }));
        }
    } else if custom.is_empty() {
        if let Some(object) = payload.as_object_mut() { object.remove("system"); }
    } else { payload["system"] = json!(custom); }
    for tool in payload["tools"].as_array_mut().into_iter().flatten() {
        if let Some(tool) = tool.as_object_mut() { tool.remove("cache_control"); }
    }
    if let Some(messages) = payload["messages"].as_array_mut() {
        for message in messages.iter_mut() {
            for block in message["content"].as_array_mut().into_iter().flatten() {
                if let Some(block) = block.as_object_mut() { block.remove("cache_control"); }
            }
        }
        if let Some(message) = messages.iter_mut().rev().find(|message| message["role"] != "system") {
            if let Some(text) = message["content"].as_str() {
                message["content"] = json!([{ "type": "text", "text": text, "cache_control": { "type": "ephemeral" } }]);
            } else if let Some(blocks) = message["content"].as_array_mut() {
                if let Some(block) = blocks.iter_mut().rev().find(|block| !matches!(block["type"].as_str(), Some("thinking" | "redacted_thinking"))) {
                    block["cache_control"] = json!({ "type": "ephemeral" });
                }
            }
        }
    }
    if !payload["metadata"].is_object() { payload["metadata"] = json!({}); }
    payload["metadata"]["user_id"] = json!(context.metadata_user);
    Ok(())
}

pub fn info() -> Result<Value> {
    let data = data()?;
    let blocks = json!({ "identity": text(&data, "cliPrefix"), "stable": stable(&data), "dynamic": dynamic(&data, false) });
    let chars = ["identity", "stable", "dynamic"].iter().map(|key| blocks[*key].as_str().unwrap_or("").chars().count()).sum::<usize>();
    Ok(json!({ "gatewayNote": "套餐池固定三块提示；客户端正文追加到动态段。WASM 使用中性环境字段，不读取本机路径。3012 不等于额度不足。",
        "gatewayChars": chars, "gatewayBlocks": blocks }))
}
