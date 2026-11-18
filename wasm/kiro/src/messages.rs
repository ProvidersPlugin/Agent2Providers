//! Chat历史 -> Kiro userInputMessage/assistantResponseMessage，保留原生配对规则。
//! 内容只做协议需要的合并，不在字符中间截断工具结果，不把坏参数或未知内容静默丢弃。

use crate::{image, tools, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

const PROCEED: &str = "Please proceed with the task.";
const NO_RESULT: &str = "Tool use was interrupted and did not produce a result.";
#[derive(Default)]
pub struct User {
    pub content: String,
    pub images: Vec<String>,
    pub results: Vec<Value>,
    pub tools: Vec<Value>,
}
#[derive(Default)]
pub struct Assistant {
    pub content: String,
    pub calls: Vec<Value>,
}
pub enum Entry {
    User(User),
    Assistant(Assistant),
}
pub fn join(left: &str, right: &str) -> String {
    if left.is_empty() {
        right.into()
    } else if right.is_empty() {
        left.into()
    } else {
        format!("{left}\n\n{right}")
    }
}
fn content(value: &Value, accept_images: bool) -> Result<(String, Vec<String>)> {
    if value.is_null() {
        return Ok((String::new(), Vec::new()));
    }
    if let Some(text) = value.as_str() {
        return Ok((text.into(), Vec::new()));
    }
    let parts = value
        .as_array()
        .ok_or_else(|| Error::http(400, "Kiro消息内容必须为文本或内容块数组"))?;
    let mut text = Vec::new();
    let mut images = Vec::new();
    for part in parts {
        match part["type"].as_str() {
            Some("text" | "input_text" | "output_text") => text.push(
                part["text"]
                    .as_str()
                    .ok_or_else(|| Error::http(400, "Kiro文本内容块无效"))?
                    .to_string(),
            ),
            Some("image_url" | "input_image") if accept_images => {
                let url = part
                    .pointer("/image_url/url")
                    .and_then(Value::as_str)
                    .or_else(|| part["image_url"].as_str())
                    .filter(|url| !url.is_empty())
                    .ok_or_else(|| Error::http(400, "Kiro图片内容块缺少地址"))?;
                images.push(url.into());
            }
            Some("image_url" | "input_image") => {
                return Err(Error::http(
                    400,
                    "Kiro系统或助手历史没有图片字段，未丢弃图片",
                ))
            }
            _ => return Err(Error::http(400, "Kiro消息含尚未支持的内容类型，未静默丢弃")),
        }
    }
    Ok((text.join("\n\n"), images))
}
fn assistant(message: &Value) -> Result<Assistant> {
    let mut out = Assistant {
        content: content(&message["content"], false)?.0,
        calls: Vec::new(),
    };
    if let Some(value) = message.get("tool_calls").filter(|value| !value.is_null()) {
        let calls = value
            .as_array()
            .ok_or_else(|| Error::http(400, "Kiro工具历史必须为数组"))?;
        for call in calls {
            if call
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind != "function")
            {
                return Err(Error::http(400, "Kiro工具历史仅接受函数调用"));
            }
            let name = call["function"]["name"]
                .as_str()
                .filter(|value| util::identifier(value, 1024))
                .ok_or_else(|| Error::http(400, "Kiro工具历史缺少有效名称"))?;
            let id = call["id"]
                .as_str()
                .filter(|value| util::identifier(value, 1024))
                .ok_or_else(|| Error::http(400, "Kiro工具历史缺少有效编号"))?;
            out.calls.push(
                json!({ "name": tools::name(name), "toolUseId": tools::id(id),
                "input": tools::arguments(&call["function"]["arguments"])? }),
            );
        }
    }
    Ok(out)
}
pub fn parse(body: &Value, supports_images: Option<bool>) -> Result<(Vec<Entry>, String)> {
    let messages = body["messages"]
        .as_array()
        .filter(|list| !list.is_empty() && list.len() <= 4096)
        .ok_or_else(|| Error::http(400, "Kiro消息列表为空或超过上限"))?;
    let mut system = Vec::new();
    let mut entries = Vec::new();
    let mut call_ids = std::collections::HashSet::new();
    for message in messages {
        let role = message["role"]
            .as_str()
            .ok_or_else(|| Error::http(400, "Kiro消息缺少角色"))?;
        match role {
            "system" | "developer" => {
                let text = content(&message["content"], false)?.0;
                if !text.is_empty() {
                    system.push(text);
                }
            }
            "assistant" => {
                let next = assistant(message)?;
                for call in &next.calls {
                    if !call_ids.insert(call["toolUseId"].as_str().unwrap_or("").to_string()) {
                        return Err(Error::http(
                            400,
                            "Kiro工具历史包含重复或规整后冲突的调用编号",
                        ));
                    }
                }
                if next.content.is_empty() && next.calls.is_empty() {
                    continue;
                }
                if let Some(Entry::Assistant(previous)) = entries.last_mut() {
                    previous.content = join(&previous.content, &next.content);
                    previous.calls.extend(next.calls);
                } else {
                    entries.push(Entry::Assistant(next));
                }
            }
            "user" | "tool" => {
                let (text, images) = content(&message["content"], true)?;
                let mut next = User {
                    images,
                    ..User::default()
                };
                if role == "tool" {
                    let id = message["tool_call_id"]
                        .as_str()
                        .filter(|value| util::identifier(value, 1024))
                        .ok_or_else(|| Error::http(400, "Kiro工具结果缺少调用编号"))?;
                    let text = if text.is_empty() {
                        "(no output)".into()
                    } else {
                        text
                    };
                    next.results.push(json!({ "toolUseId": tools::id(id),
                        "status": if message["is_error"] == true { "error" } else { "success" }, "content": [{ "text": text }] }));
                } else {
                    next.content = text;
                }
                if let Some(Entry::User(previous)) = entries.last_mut() {
                    previous.content = join(&previous.content, &next.content);
                    previous.images.extend(next.images);
                    previous.results.extend(next.results);
                } else {
                    entries.push(Entry::User(next));
                }
            }
            _ => return Err(Error::http(400, "Kiro不支持此消息角色")),
        }
    }
    // Kiro 要求首尾 user；不能通过删除前置 assistant/孤立结果伪造完整多轮历史。
    if !matches!(entries.first(), Some(Entry::User(user)) if user.results.is_empty()) {
        return Err(Error::http(
            400,
            "Kiro历史缺少首条用户消息或包含孤立工具结果，未删除历史",
        ));
    }
    if !matches!(entries.last(), Some(Entry::User(_))) {
        entries.push(Entry::User(User::default()));
    }
    for index in 0..entries.len() {
        let calls = if index > 0 {
            match &entries[index - 1] {
                Entry::Assistant(message) => message.calls.clone(),
                _ => Vec::new(),
            }
        } else {
            Vec::new()
        };
        if let Entry::User(user) = &mut entries[index] {
            let mut answers = Vec::new();
            for result in &user.results {
                let id = result["toolUseId"].as_str().unwrap_or("");
                if answers.iter().any(|old: &String| old == id)
                    || !calls.iter().any(|call| call["toolUseId"] == id)
                {
                    return Err(Error::http(
                        400,
                        "Kiro工具结果重复或没有对应调用，未静默删除结果",
                    ));
                }
                answers.push(id.to_string());
            }
            user.results.sort_by_key(|result| {
                calls
                    .iter()
                    .position(|call| call["toolUseId"] == result["toolUseId"])
                    .unwrap_or(usize::MAX)
            });
            for call in calls {
                let id = call["toolUseId"].as_str().unwrap_or("");
                if !answers.iter().any(|old| old == id) {
                    answers.push(id.to_string());
                    user.results.push(json!({ "toolUseId": id, "status": "error", "content": [{ "text": NO_RESULT }] }));
                }
            }
            if user.content.is_empty() && user.results.is_empty() {
                user.content = PROCEED.into();
            }
        }
    }
    let mut image_bytes = 0usize;
    let mut image_count = 0usize;
    for entry in &mut entries {
        if let Entry::User(user) = entry {
            image_count = image_count.saturating_add(user.images.len());
            if image_count > 20 {
                return Err(Error::http(
                    400,
                    "Kiro本轮完整历史图片数量超过上限，未静默丢图",
                ));
            }
            if !user.images.is_empty() && supports_images == Some(false) {
                return Err(Error::http(
                    400,
                    "当前Kiro在线模型目录明确不支持图片，未下载或丢弃图片",
                ));
            }
            for url in &mut user.images {
                *url = image::prepare(url)?;
                image_bytes = image_bytes.saturating_add(url.len());
                if image_bytes > 8 * 1024 * 1024 {
                    return Err(Error::http(
                        413,
                        "Kiro本轮完整历史图片总量超过受控上限，未静默丢图",
                    ));
                }
            }
        }
    }
    Ok((entries, system.join("\n\n")))
}
pub fn user_value(user: &User, model: &str) -> Value {
    let mut value = json!({ "content": user.content, "modelId": model, "origin": "KIRO_CLI" });
    if !user.images.is_empty() {
        value["images"] = json!(user
            .images
            .iter()
            .filter_map(|url| {
                let (meta, data) = url.strip_prefix("data:image/")?.split_once(";base64,")?;
                Some(json!({ "format": meta, "source": { "bytes": data } }))
            })
            .collect::<Vec<_>>());
    }
    let mut context = json!({});
    if !user.tools.is_empty() {
        context["tools"] = json!(user.tools);
    }
    if !user.results.is_empty() {
        context["toolResults"] = json!(user.results);
    }
    if context.as_object().is_some_and(|value| !value.is_empty()) {
        value["userInputMessageContext"] = context;
    }
    value
}
pub fn assistant_value(message: &Assistant) -> Value {
    let mut value = json!({ "content": message.content });
    if !message.calls.is_empty() {
        value["toolUses"] = json!(message.calls);
    }
    value
}
