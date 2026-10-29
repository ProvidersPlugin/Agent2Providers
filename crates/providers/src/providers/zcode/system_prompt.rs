//! Start Plan 的系统提示词、缓存断点和上下文前缀。
//! 装配开启时固定官方三块，客户端系统正文合入已有用户消息。

use std::sync::OnceLock;

use serde_json::{json, Map, Value};

use crate::errors::GatewayError;

use super::region::Region;
use super::request_context::RequestContext;

const SYSTEM_JSON: &str = include_str!("zcode_system.json");
static SYSTEM_DATA: OnceLock<Option<Value>> = OnceLock::new();

#[derive(Clone, Debug, Default)]
pub struct GatewayBlocks {
    pub identity: String,
    pub stable: String,
    pub dynamic: String,
}

impl GatewayBlocks {
    pub const FIELDS: [&'static str; 3] = ["identity", "stable", "dynamic"];

    pub fn get(&self, field: &str) -> &str {
        match field {
            "identity" => &self.identity,
            "stable" => &self.stable,
            "dynamic" => &self.dynamic,
            _ => "",
        }
    }

    pub fn set(&mut self, field: &str, value: String) {
        match field {
            "identity" => self.identity = value,
            "stable" => self.stable = value,
            "dynamic" => self.dynamic = value,
            _ => {}
        }
    }
}

fn system_data() -> Option<&'static Value> {
    SYSTEM_DATA.get_or_init(|| serde_json::from_str(SYSTEM_JSON).ok()).as_ref()
}

pub fn official_prompt_approx_chars() -> usize {
    official_blocks_template()
        .map(|blocks| blocks.identity.chars().count() + blocks.stable.chars().count() + blocks.dynamic.chars().count())
        .unwrap_or(0)
}

pub fn official_blocks_template() -> Option<GatewayBlocks> {
    let data = system_data()?;
    Some(GatewayBlocks {
        identity: text_of(data, "cliPrefix"),
        stable: stable_joined(data),
        dynamic: [dynamic_before(data), environment_template(data), dynamic_after(data)].join("\n\n"),
    })
}

pub(super) fn apply_start_plan(
    payload: &mut Value,
    model: &str,
    region: Region,
    device_mid: &str,
    official_prompt: bool,
    override_blocks: Option<&GatewayBlocks>,
    request_context: &RequestContext,
) -> Result<(), GatewayError> {
    let data = if official_prompt {
        Some(system_data().ok_or_else(|| GatewayError::with_status(
            500,
            "内置的 ZCode 官方提示词资源不可用（解析失败）：请重新安装或升级应用",
        ))?)
    } else {
        None
    };
    let object = payload.as_object_mut().ok_or_else(|| {
        GatewayError::with_status(500, "请求体形态异常（不是对象）")
    })?;
    if let Some(data) = data {
        let texts = merged_texts(data, model, region, override_blocks);
        let custom = custom_system_text(object.get("system"));
        object.insert("system".to_string(), Value::Array(system_blocks(&texts)));
        if let Some(custom) = custom {
            prepend_folded_system(object, custom)?;
        }
        if let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) {
            if !messages.is_empty() {
                messages.insert(0, context_prefix_message(data));
            }
        }
    } else if let Some(existing) = object.get("system") {
        let blocks = user_system_blocks(Some(existing));
        if blocks.is_empty() {
            object.remove("system");
        } else {
            object.insert("system".to_string(), Value::Array(blocks));
        }
    }
    strip_tool_cache_control(object);
    apply_cache_control(object);
    if let Some(user_id) = request_context.metadata_user_id(device_mid) {
        let mut metadata = object.get("metadata")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        metadata.insert("user_id".to_string(), Value::String(user_id));
        object.insert("metadata".to_string(), Value::Object(metadata));
    }
    Ok(())
}

fn merged_texts(
    data: &Value,
    model: &str,
    region: Region,
    override_blocks: Option<&GatewayBlocks>,
) -> GatewayBlocks {
    let values = template_values(data, model, region);
    let defaults = GatewayBlocks {
        identity: text_of(data, "cliPrefix"),
        stable: stable_joined(data),
        dynamic: [dynamic_before(data), substitute(&environment_template(data), &values), dynamic_after(data)].join("\n\n"),
    };
    let Some(overrides) = override_blocks else { return defaults };
    let pick = |field| {
        let text = overrides.get(field);
        if text.trim().is_empty() {
            defaults.get(field).to_string()
        } else {
            substitute(text, &values)
        }
    };
    GatewayBlocks {
        identity: pick("identity"),
        stable: pick("stable"),
        dynamic: pick("dynamic"),
    }
}

fn text_of(data: &Value, key: &str) -> String {
    data.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn stable_joined(data: &Value) -> String {
    data.get("stableSections")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n\n"))
        .unwrap_or_default()
}

fn dynamic_before(data: &Value) -> String {
    data.get("dynamicSections").map(|value| text_of(value, "beforeEnvironment")).unwrap_or_default()
}

fn dynamic_after(data: &Value) -> String {
    data.get("dynamicSections").map(|value| text_of(value, "afterEnvironment")).unwrap_or_default()
}

fn template_values(data: &Value, model: &str, region: Region) -> Vec<(&'static str, String)> {
    let git = data.pointer("/environment/gitNo").and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty()).unwrap_or("no");
    vec![
        ("{cwd}", working_dir()),
        ("{git}", git.to_string()),
        ("{platform}", os_platform().to_string()),
        ("{shell}", shell_name()),
        ("{os_version}", format!("{} {}", os_platform(), os_arch())),
        ("{provider}", match region { Region::Cn => "bigmodel-api", Region::Intl => "zai-api" }.to_string()),
        ("{model}", model.to_string()),
    ]
}

fn substitute(text: &str, values: &[(&'static str, String)]) -> String {
    let mut out = text.to_string();
    for (token, value) in values {
        out = out.replace(token, value);
    }
    out
}

fn system_blocks(texts: &GatewayBlocks) -> Vec<Value> {
    let ephemeral = json!({ "type": "ephemeral" });
    vec![
        text_block(texts.identity.clone(), Some(ephemeral.clone())),
        text_block(texts.stable.clone(), Some(ephemeral.clone())),
        text_block(format!("\n\n{}", texts.dynamic), Some(ephemeral)),
    ]
}

fn custom_system_text(existing: Option<&Value>) -> Option<String> {
    let parts = user_system_blocks(existing);
    let text = parts.iter().filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>().join("\n\n");
    (!text.is_empty()).then_some(text)
}

fn user_system_blocks(existing: Option<&Value>) -> Vec<Value> {
    let Some(existing) = existing else { return Vec::new() };
    let items = match existing {
        Value::String(text) => return if text.trim().is_empty() {
            Vec::new()
        } else {
            vec![text_block(text.clone(), None)]
        },
        Value::Array(items) => items,
        _ => return Vec::new(),
    };
    items.iter().filter_map(|item| {
        let text = item.as_str().or_else(|| {
            (item.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| item.get("text").and_then(Value::as_str)).flatten()
        })?;
        (!text.trim().is_empty()).then(|| text_block(text.to_string(), None))
    }).collect()
}

const FOLDED_SYSTEM_MARKER: &str = "请依据下面的会话要求处理用户消息。两者冲突时，以会话要求为准。\n\n会话要求：\n\n";

fn prepend_folded_system(object: &mut Map<String, Value>, custom: String) -> Result<(), GatewayError> {
    let message = object.get_mut("messages").and_then(Value::as_array_mut)
        .and_then(|messages| messages.iter_mut().find(|message| {
            message.get("role").and_then(Value::as_str) == Some("user")
        })).ok_or_else(|| GatewayError::with_status(
            400,
            "套餐池请求带有系统提示词，但没有用户消息可承载会话要求；请至少保留一条用户消息",
        ))?;
    let prefix = format!("{FOLDED_SYSTEM_MARKER}{custom}\n\n用户消息：\n\n");
    // 上游按会话结构校验；折叠只能修改已有消息，不能再增加一条 user。
    match message.get_mut("content") {
        Some(Value::String(text)) => *text = format!("{prefix}{text}"),
        Some(Value::Array(blocks)) => {
            if let Some(Value::Object(block)) = blocks.iter_mut().find(|block| {
                block.get("type").and_then(Value::as_str) == Some("text")
                    && block.get("text").and_then(Value::as_str).is_some()
            }) {
                if let Some(Value::String(text)) = block.get_mut("text") {
                    *text = format!("{prefix}{text}");
                }
            } else {
                blocks.push(text_block(prefix, None));
            }
        }
        _ => message["content"] = Value::Array(vec![text_block(prefix, None)]),
    }
    Ok(())
}

fn text_block(text: String, cache_control: Option<Value>) -> Value {
    let mut block = json!({ "type": "text", "text": text });
    if let Some(cache) = cache_control {
        block["cache_control"] = cache;
    }
    block
}

fn environment_template(data: &Value) -> String {
    let env = data.get("environment").unwrap_or(&Value::Null);
    [
        text_of(env, "heading"),
        text_of(env, "invokedLine"),
        format!("- {}: {{cwd}}", text_of(env, "cwdLabel")),
        format!("- {}: {{git}}", text_of(env, "gitLabel")),
        format!("- {}: {{platform}}", text_of(env, "platformLabel")),
        format!("- {}: {{shell}}", text_of(env, "shellLabel")),
        format!("- {}: {{os_version}}", text_of(env, "osVersionLabel")),
        text_of(env, "poweredByLine"),
    ].into_iter().filter(|line| !line.trim().is_empty()).collect::<Vec<_>>().join("\n")
}

fn os_platform() -> &'static str {
    if cfg!(target_os = "windows") { "win32" } else if cfg!(target_os = "macos") { "darwin" } else { "linux" }
}

fn os_arch() -> &'static str {
    if cfg!(target_arch = "x86_64") { "x64" } else if cfg!(target_arch = "aarch64") { "arm64" } else { "unknown" }
}

fn working_dir() -> String {
    std::env::current_dir().ok().map(|path| path.to_string_lossy().to_string())
        .filter(|value| !value.trim().is_empty()).unwrap_or_else(|| ".".to_string())
}

fn shell_name() -> String {
    let raw = std::env::var("SHELL").or_else(|_| std::env::var("ComSpec"))
        .or_else(|_| std::env::var("COMSPEC")).unwrap_or_default();
    let name = raw.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if name.is_empty() { "unknown".to_string() } else { name.to_string() }
}

fn context_prefix_message(data: &Value) -> Value {
    let context = data.get("contextPrefix").unwrap_or(&Value::Null);
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let section = format!("{}\n{}", text_of(context, "currentDateHeading"),
        text_of(context, "currentDateLine").replace("{date}", &date));
    let body = [text_of(context, "intro"), section, String::new(), text_of(context, "outro")].join("\n");
    let open = data.pointer("/systemReminder/open").and_then(Value::as_str).unwrap_or("<system-reminder>");
    let close = data.pointer("/systemReminder/close").and_then(Value::as_str).unwrap_or("</system-reminder>");
    json!({ "role": "user", "content": [{ "type": "text", "text": format!("{open}{body}{close}") }] })
}

fn strip_tool_cache_control(object: &mut Map<String, Value>) {
    if let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if let Some(tool) = tool.as_object_mut() { tool.remove("cache_control"); }
        }
    }
}

fn apply_cache_control(object: &mut Map<String, Value>) {
    let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) else { return };
    for message in messages.iter_mut() {
        if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
            for block in blocks {
                if let Some(block) = block.as_object_mut() { block.remove("cache_control"); }
            }
        }
    }
    for message in messages.iter_mut().rev() {
        if message.get("role").and_then(Value::as_str) == Some("system") { continue }
        if let Some(Value::String(text)) = message.get("content") {
            message["content"] = Value::Array(vec![text_block(text.clone(), Some(json!({ "type": "ephemeral" })))]);
        } else if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
            if let Some(block) = blocks.iter_mut().rev().find(|block| {
                block.get("type").and_then(Value::as_str).is_some_and(|kind| kind != "thinking" && kind != "redacted_thinking")
            }).and_then(Value::as_object_mut) {
                block.insert("cache_control".to_string(), json!({ "type": "ephemeral" }));
            }
        }
        return;
    }
}
