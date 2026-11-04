//! OpenAI Chat 请求 → Kiro 的 `generateAssistantResponse` 会话。
//!
//! ── 两套消息模型怎么对上 ────────────────────────────────────
//! ```text
//!   OpenAI Chat                       Kiro conversationState
//!   system / developer          →     拼在第一条 user 的正文前面（Kiro 不收
//!                                     systemPrompt 字段）
//!   user（text / image_url）    →     userInputMessage（content / images）
//!   assistant（content）        →     assistantResponseMessage.content
//!   assistant.tool_calls        →     assistantResponseMessage.toolUses
//!   tool（tool_call_id）        →     下一条 userInputMessage 的 toolResults
//!   tools[].function            →     toolSpecification
//! ```
//!
//! ── 几条硬规则（都是上游 400 教出来的）──────────────────────
//!   1. **首尾必须是 userInputMessage**：开头的 assistant / 只有工具结果的
//!      user 会被删掉，结尾是 assistant 时补一条空 user；
//!   2. **每个 tool call 必须有 result**：缺的补一条 error 结果；多出来的
//!      result 丢掉 —— Kiro 会校验配对；
//!   3. **工具名与 id 的字符集**：名字只收 `[a-zA-Z0-9_-]{1,64}`，id 只收
//!      `[a-zA-Z0-9_.:-]{1,64}`；不合规的按哈希改名，结果回给客户端时仍用
//!      客户端原来的名字（由 `tool_names` 映射回来）；
//!   4. **工具参数 schema 的根必须是 object**：`{}`、无 type、object-or-null
//!      都要补成 object；
//!   5. **只带最新一组图片**：历史里的旧图每次都重传会白烧额度；
//!   6. **空消息用 `Please proceed with the task.`**：Kiro 不接受空 content。
//!
//! ── 思考 ────────────────────────────────────────────────────
//! 只对 Claude 与 Auto 生效。具体走哪条路由由 `native_of` 决定：
//! 目录声明了自适应思考的模型发 `additionalModelRequestFields`，其余发
//! `<thinking_mode>` + `<max_thinking_length>` 前缀（与 kiro-cli 同款）。

use serde_json::{json, Map, Value};

use super::models::NativeEffort;

/// Kiro 不接受空消息时的替代正文。
const PROCEED: &str = "Please proceed with the task.";
/// 被截断的工具输出的占位说明。
const NO_RESULT: &str = "Tool use was interrupted and did not produce a result.";
/// 工具结果的长度上限。
const RESULT_LIMIT: usize = 250_000;

/// 一条 user 消息。
#[derive(Default, Clone)]
struct UserMessage {
    content: String,
    images: Vec<Value>,
    tool_results: Vec<Value>,
    tools: Vec<Value>,
    model_id: String,
}

/// 一条 assistant 消息。
#[derive(Default, Clone)]
struct AssistantMessage {
    content: String,
    tool_uses: Vec<Value>,
}

#[derive(Default, Clone)]
struct Entry {
    user: Option<UserMessage>,
    assistant: Option<AssistantMessage>,
}

/// 思考档位（OpenAI 的 `reasoning_effort` 与 Anthropic 的 budget 都折到这里）。
pub fn thinking_budget(body: &Value, model: &str) -> Option<u32> {
    if !super::models::thinks(&model.to_ascii_lowercase()) {
        return None;
    }
    let effort = body
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .or_else(|| body.pointer("/reasoning/effort").and_then(Value::as_str))
        .unwrap_or("");
    let budget = match effort {
        "low" => 10_000,
        "medium" => 20_000,
        "high" => 30_000,
        "xhigh" | "max" => 50_000,
        _ => 0,
    };
    if budget > 0 {
        return Some(budget);
    }
    // 客户端直接给了 thinking 预算（Anthropic 风格扩展字段）
    body.pointer("/thinking/budget_tokens")
        .and_then(Value::as_u64)
        .map(|value| value.clamp(1024, 50_000) as u32)
}

/// 结构化输出：OpenAI 的 `response_format` 用一句话告诉模型。
fn format_text(body: &Value) -> String {
    let format = body
        .get("response_format")
        .or_else(|| body.get("output_format"));
    let Some(format) = format else {
        return String::new();
    };
    let kind = format.get("type").and_then(Value::as_str).unwrap_or("");
    if kind == "json_object" {
        return "Respond with a single JSON object and nothing else.".to_string();
    }
    let schema = format
        .get("json_schema")
        .and_then(|value| value.get("schema"))
        .or_else(|| format.get("schema"));
    let Some(schema) = schema.filter(|value| value.is_object()) else {
        return String::new();
    };
    format!(
        "Respond with a single JSON value and nothing else, matching this JSON schema:\n{}.",
        serde_json::to_string(schema).unwrap_or_default()
    )
}

/// 工具名规整（Kiro 只收 `[a-zA-Z0-9_-]{1,64}`）。
pub fn tool_name(name: &str) -> String {
    if is_tool_name(name) {
        return name.to_string();
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(name.as_bytes());
    let hex: String = digest
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let sanitized: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let keep = 64usize.saturating_sub(1 + hex.len());
    let head: String = sanitized.chars().take(keep).collect();
    format!("{head}_{hex}")
}

fn is_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// 工具调用 id 规整（Kiro 只收 `[a-zA-Z0-9_.:-]{1,64}`）。
pub fn tool_id(id: &str) -> String {
    if is_tool_id(id) {
        return id.to_string();
    }
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(id.as_bytes());
    let encoded = URL_SAFE_NO_PAD.encode(digest);
    format!("t_{}", encoded.chars().take(32).collect::<String>())
}

fn is_tool_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | ':' | '-'))
}

/// 名字 → 客户端原来的名字（Kiro 回的是规整后的名字）。
pub fn tool_names(body: &Value) -> std::collections::HashMap<String, String> {
    let mut names = std::collections::HashMap::new();
    let mut add = |name: &str| {
        let mapped = tool_name(name);
        if mapped != name {
            names.insert(mapped, name.to_string());
        }
    };
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for tool in tools {
            if let Some(name) = tool
                .pointer("/function/name")
                .and_then(Value::as_str)
                .or_else(|| tool.get("name").and_then(Value::as_str))
            {
                add(name);
            }
        }
    }
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for message in messages {
            for call in message
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                    add(name);
                }
            }
        }
    }
    names
}

/// 文本内容（字符串 / OpenAI 内容块数组）。
fn content_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    let Some(parts) = content.as_array() else {
        return String::new();
    };
    parts
        .iter()
        .filter_map(|part| match part.get("type").and_then(Value::as_str) {
            Some("text" | "input_text" | "output_text") => {
                part.get("text").and_then(Value::as_str).map(str::to_string)
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// 工具参数：OpenAI 的 `arguments` 是 JSON 字符串，Kiro 要对象。
fn arguments_value(raw: &Value) -> Value {
    if let Some(text) = raw.as_str() {
        serde_json::from_str::<Value>(text)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}))
    } else if raw.is_object() {
        raw.clone()
    } else {
        json!({})
    }
}

/// 工具参数 schema 的根补成 object（Kiro 的硬要求）。
pub fn input_schema(schema: Option<&Value>) -> Value {
    let Some(schema) = schema.filter(|value| value.is_object()) else {
        return json!({ "type": "object", "properties": {} });
    };
    let object = schema.as_object().cloned().unwrap_or_default();
    if object.is_empty() {
        return json!({ "type": "object", "properties": {} });
    }
    match object.get("type") {
        None => {
            let mut next = Map::new();
            next.insert("type".into(), json!("object"));
            for (key, value) in object {
                next.insert(key, value);
            }
            Value::Object(next)
        }
        Some(Value::Array(types)) => {
            let nullable_object = !types.is_empty()
                && types
                    .iter()
                    .all(|kind| kind.as_str() == Some("object") || kind.as_str() == Some("null"));
            if nullable_object {
                let mut next = object;
                next.insert("type".into(), json!("object"));
                Value::Object(next)
            } else {
                schema.clone()
            }
        }
        _ => schema.clone(),
    }
}

fn join_non_empty(left: &str, right: &str) -> String {
    if left.is_empty() {
        right.to_string()
    } else if right.is_empty() {
        left.to_string()
    } else {
        format!("{left}\n\n{right}")
    }
}

/// 一张图片（OpenAI 的 `image_url`）：只有内联 base64 能发，URL 没有字节。
fn push_image(images: &mut Vec<Value>, url: &str) {
    let Some((format, data)) = super::image::split_data_url(url) else {
        return;
    };
    if data.is_empty() {
        return;
    }
    if data.len() <= super::image::IMAGE_LIMIT {
        images.push(json!({ "format": format, "source": { "bytes": data } }));
        return;
    }
    match super::image::fit(&data) {
        super::image::Fitted::Smaller { format, bytes } => {
            images.push(json!({ "format": format, "source": { "bytes": bytes } }));
        }
        super::image::Fitted::Note(note) => {
            images.push(json!({
                "format": "png",
                "note": format!(
                    "[一张图片（{format}，Base64 {:.1} MB）没有随本条消息发送：Kiro 单张上限 {:.1} MB，{note}。]",
                    data.len() as f64 / (1024.0 * 1024.0),
                    super::image::IMAGE_LIMIT as f64 / (1024.0 * 1024.0),
                )
            }));
        }
    }
}

/// 构造请求体。
///
/// `model` 是网关解析出的发送名；`profile` 为空时（API key）不写 profileArn。
pub fn build(
    body: &Value,
    model: &str,
    profile: &str,
    budget: Option<u32>,
    native: Option<&NativeEffort>,
) -> Value {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut system_parts: Vec<String> = Vec::new();
    let mut entries: Vec<Entry> = Vec::new();
    for message in &messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        if role == "system" || role == "developer" {
            let text = content_text(message.get("content").unwrap_or(&Value::Null));
            if !text.trim().is_empty() {
                system_parts.push(text);
            }
            continue;
        }
        if role == "assistant" {
            let mut assistant = AssistantMessage {
                content: content_text(message.get("content").unwrap_or(&Value::Null)),
                tool_uses: Vec::new(),
            };
            for call in message
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let name = call
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if name.is_empty() {
                    continue;
                }
                assistant.tool_uses.push(json!({
                    "name": tool_name(name),
                    "toolUseId": tool_id(
                        call.get("id").and_then(Value::as_str).unwrap_or(name)
                    ),
                    "input": arguments_value(
                        call.pointer("/function/arguments").unwrap_or(&Value::Null)
                    ),
                }));
            }
            if assistant.content.is_empty() && assistant.tool_uses.is_empty() {
                continue;
            }
            if let Some(previous) = entries.last_mut().and_then(|entry| entry.assistant.take()) {
                let mut merged = previous;
                merged.content = join_non_empty(&merged.content, &assistant.content);
                merged.tool_uses.extend(assistant.tool_uses);
                if let Some(last) = entries.last_mut() {
                    last.assistant = Some(merged);
                }
            } else {
                entries.push(Entry {
                    assistant: Some(assistant),
                    user: None,
                });
            }
            continue;
        }
        // user / tool
        let mut user = UserMessage {
            content: String::new(),
            images: Vec::new(),
            tool_results: Vec::new(),
            tools: Vec::new(),
            model_id: model.to_string(),
        };
        if role == "tool" {
            let call_id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let mut output = content_text(message.get("content").unwrap_or(&Value::Null));
            if output.is_empty() {
                output = "(no output)".to_string();
            }
            if output.len() > RESULT_LIMIT {
                output.truncate(RESULT_LIMIT);
                output.push_str("\n… (cut)");
            }
            user.tool_results.push(json!({
                "toolUseId": tool_id(call_id),
                "status": if message.get("is_error").and_then(Value::as_bool) == Some(true) {
                    "error"
                } else {
                    "success"
                },
                "content": [{ "text": output }],
            }));
            // 工具结果里附带的图片（截图类客户端）跟着图片字段走。
            if let Some(parts) = message.get("content").and_then(Value::as_array) {
                for part in parts {
                    if part.get("type").and_then(Value::as_str) == Some("image_url") {
                        if let Some(url) = part.pointer("/image_url/url").and_then(Value::as_str) {
                            push_image(&mut user.images, url);
                        }
                    }
                }
            }
        } else {
            user.content = content_text(message.get("content").unwrap_or(&Value::Null));
            if let Some(parts) = message.get("content").and_then(Value::as_array) {
                for part in parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some("image_url") => {
                            if let Some(url) =
                                part.pointer("/image_url/url").and_then(Value::as_str)
                            {
                                push_image(&mut user.images, url);
                            }
                        }
                        Some("input_image") => {
                            if let Some(url) = part.get("image_url").and_then(Value::as_str) {
                                push_image(&mut user.images, url);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        if let Some(previous) = entries.last_mut().and_then(|entry| entry.user.take()) {
            let mut merged = previous;
            merged.content = join_non_empty(&merged.content, &user.content);
            merged.images.extend(user.images);
            merged.tool_results.extend(user.tool_results);
            if let Some(last) = entries.last_mut() {
                last.user = Some(merged);
            }
        } else {
            entries.push(Entry {
                user: Some(user),
                assistant: None,
            });
        }
    }

    // 首条必须是 userInputMessage：开头的 assistant 与「只有工具结果」的 user 删掉。
    while entries.first().is_some_and(|entry| entry.user.is_none()) {
        entries.remove(0);
    }
    while entries.first().is_some_and(|entry| {
        entry
            .user
            .as_ref()
            .is_some_and(|user| !user.tool_results.is_empty() && user.content.is_empty())
    }) {
        entries.remove(0);
    }
    if let Some(first) = entries.first_mut().and_then(|entry| entry.user.as_mut()) {
        first.tool_results.clear();
    }
    // 末条必须是 userInputMessage（它是要被回答的那条）。
    let needs_user = entries
        .last()
        .map(|entry| entry.user.is_none())
        .unwrap_or(true);
    if needs_user {
        entries.push(Entry {
            user: Some(UserMessage {
                model_id: model.to_string(),
                ..Default::default()
            }),
            assistant: None,
        });
    }
    // 工具调用与结果配对。
    let mut index = 0;
    while index < entries.len() {
        let has_user = entries[index].user.is_some();
        if !has_user {
            index += 1;
            continue;
        }
        let calls = if index > 0 {
            entries[index - 1]
                .assistant
                .as_ref()
                .map(|assistant| assistant.tool_uses.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let Some(user) = entries[index].user.as_mut() else {
            index += 1;
            continue;
        };
        let mut answered: Vec<String> = Vec::new();
        let mut kept: Vec<Value> = Vec::new();
        for result in std::mem::take(&mut user.tool_results) {
            let id = result
                .get("toolUseId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !answered.contains(&id)
                && calls
                    .iter()
                    .any(|call| call.get("toolUseId").and_then(Value::as_str) == Some(id.as_str()))
            {
                answered.push(id);
                kept.push(result);
            }
        }
        for call in &calls {
            let id = call
                .get("toolUseId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !answered.contains(&id) {
                answered.push(id.clone());
                kept.push(json!({
                    "toolUseId": id,
                    "status": "error",
                    "content": [{ "text": NO_RESULT }],
                }));
            }
        }
        user.tool_results = kept;
        index += 1;
    }
    // 空消息补一句（Kiro 拒绝空 content）。
    for entry in &mut entries {
        if let Some(user) = entry.user.as_mut() {
            if user.content.is_empty() && user.tool_results.is_empty() {
                user.content = PROCEED.to_string();
            }
        }
    }
    // 只保留最新一组图片。
    let latest_images = entries.iter().rposition(|entry| {
        entry
            .user
            .as_ref()
            .is_some_and(|user| !user.images.is_empty())
    });
    for (position, entry) in entries.iter_mut().enumerate() {
        if let Some(user) = entry.user.as_mut() {
            if Some(position) != latest_images {
                user.images.clear();
            }
        }
    }

    // 指令：拼到第一条 user 的正文前面。
    let mut system = system_parts.join("\n\n");
    let format = format_text(body);
    if !format.is_empty() {
        system = join_non_empty(&system, &format);
    }
    let native_effort = effort_of(body);
    let use_native = budget.is_some()
        && native.is_some_and(|native| {
            native_effort
                .as_deref()
                .is_some_and(|effort| native.levels.iter().any(|level| level == effort))
        });
    if budget.is_some() && !use_native {
        let prefix = format!(
            "<thinking_mode>enabled</thinking_mode><max_thinking_length>{}</max_thinking_length>",
            budget.unwrap_or(0)
        );
        system = if system.is_empty() {
            prefix
        } else {
            format!("{prefix}\n{system}")
        };
    }
    if let Some(first) = entries.first_mut().and_then(|entry| entry.user.as_mut()) {
        if !system.is_empty() {
            first.content = format!("{system}\n\n{}", first.content);
        }
    }

    // 工具声明：客户端给的，加上历史里用到但这次没给的（Kiro 会拒绝未知工具）。
    let mut tools: Vec<Value> = Vec::new();
    let mut offered: Vec<String> = Vec::new();
    if let Some(list) = body.get("tools").and_then(Value::as_array) {
        for tool in list {
            let function = tool.get("function").unwrap_or(tool);
            let Some(name) = function.get("name").and_then(Value::as_str) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            let mapped = tool_name(name);
            if offered.contains(&mapped) {
                continue;
            }
            offered.push(mapped.clone());
            tools.push(json!({
                "toolSpecification": {
                    "name": mapped,
                    "description": function
                        .get("description")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .unwrap_or(name),
                    "inputSchema": {
                        "json": input_schema(function.get("parameters").or_else(|| function.get("input_schema")))
                    }
                }
            }));
        }
    }
    for entry in &entries {
        if let Some(assistant) = entry.assistant.as_ref() {
            for call in &assistant.tool_uses {
                let Some(name) = call.get("name").and_then(Value::as_str) else {
                    continue;
                };
                if offered.iter().any(|item| item == name) {
                    continue;
                }
                offered.push(name.to_string());
                tools.push(json!({
                    "toolSpecification": {
                        "name": name,
                        "description": "Tool",
                        "inputSchema": { "json": { "type": "object", "properties": {} } }
                    }
                }));
            }
        }
    }
    if let Some(current) = entries.last_mut().and_then(|entry| entry.user.as_mut()) {
        if !tools.is_empty() {
            current.tools = tools;
        }
    }

    // 落成 JSON。
    let mut history: Vec<Value> = Vec::new();
    let last = entries.pop();
    for entry in entries {
        if let Some(user) = entry.user {
            history.push(json!({ "userInputMessage": user_json(&user) }));
        } else if let Some(assistant) = entry.assistant {
            history.push(json!({ "assistantResponseMessage": assistant_json(&assistant) }));
        }
    }
    let current = last
        .and_then(|entry| entry.user)
        .unwrap_or_else(|| UserMessage {
            model_id: model.to_string(),
            ..Default::default()
        });
    let mut state = json!({
        "chatTriggerType": "MANUAL",
        "agentTaskType": "vibe",
        "conversationId": crate::ids::uuid_v4(),
        "currentMessage": { "userInputMessage": user_json(&current) },
    });
    if !history.is_empty() {
        state["history"] = json!(history);
    }
    let mut out = json!({ "conversationState": state, "agentMode": "vibe" });
    if !profile.is_empty() {
        out["profileArn"] = json!(profile);
    }
    if use_native {
        if let (Some(native), Some(effort)) = (native, native_effort) {
            out["additionalModelRequestFields"] = json!({
                "thinking": { "type": "adaptive" },
                native.path.clone(): { "effort": effort },
            });
        }
    }
    out
}

fn effort_of(body: &Value) -> Option<String> {
    body.get("reasoning_effort")
        .and_then(Value::as_str)
        .or_else(|| body.pointer("/reasoning/effort").and_then(Value::as_str))
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn user_json(user: &UserMessage) -> Value {
    let mut object = Map::new();
    object.insert("content".into(), json!(user.content));
    object.insert("modelId".into(), json!(user.model_id));
    object.insert("origin".into(), json!("KIRO_CLI"));
    if !user.images.is_empty() {
        let images: Vec<Value> = user
            .images
            .iter()
            .filter(|image| image.get("note").is_none())
            .cloned()
            .collect();
        let notes: Vec<String> = user
            .images
            .iter()
            .filter_map(|image| {
                image
                    .get("note")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        if !images.is_empty() {
            object.insert("images".into(), json!(images));
        }
        if !notes.is_empty() {
            let content = object
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            object.insert(
                "content".into(),
                json!(join_non_empty(&content, &notes.join("\n"))),
            );
        }
    }
    let mut context = Map::new();
    if !user.tools.is_empty() {
        context.insert("tools".into(), json!(user.tools));
    }
    if !user.tool_results.is_empty() {
        context.insert("toolResults".into(), json!(user.tool_results));
    }
    if !context.is_empty() {
        object.insert("userInputMessageContext".into(), Value::Object(context));
    }
    Value::Object(object)
}

fn assistant_json(assistant: &AssistantMessage) -> Value {
    let mut object = Map::new();
    object.insert("content".into(), json!(assistant.content));
    if !assistant.tool_uses.is_empty() {
        object.insert("toolUses".into(), json!(assistant.tool_uses));
    }
    Value::Object(object)
}
