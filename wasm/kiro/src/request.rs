//! 组合已验证的Kiro消息、工具、思考与profile；不引全原生provider依赖。
//! body与出站base64封装受16MiB ABI约束，10MiB JSON前置护栏避免封装后溢出。

use crate::{
    messages::{self, Entry},
    models, reasoning, tools, util,
};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub struct Built {
    pub body: Value,
    pub split_thinking: bool,
}
pub fn build(body: &Value, model: &str, profile: &str, record: &Value) -> Result<Built> {
    if !util::identifier(model, 256) {
        return Err(Error::http(400, "Kiro模型名无效"));
    }
    tools::validate_choice(body)?;
    let _names = tools::restore(body)?;
    let budget = reasoning::budget(body, model)?;
    let mut declarations = tools::declarations(body)?;
    let output = reasoning::output_instruction(body)?;
    let (mut entries, system) = messages::parse(body, record["supportsImages"].as_bool())?;
    let mut system = messages::join(&system, &output);
    let native = models::native(record);
    let level = reasoning::effort(body);
    let use_native = budget.is_some()
        && native
            .as_ref()
            .is_some_and(|native| native.levels.iter().any(|entry| entry == level));
    if body["thinking"]["type"] == "adaptive"
        && (!use_native || native.as_ref().is_none_or(|native| !native.adaptive))
    {
        return Err(Error::http(400, "当前Kiro模型目录没有声明此自适应思考档位"));
    }
    if let Some(budget) = budget.filter(|_| !use_native) {
        system = messages::join(&format!("<thinking_mode>enabled</thinking_mode><max_thinking_length>{budget}</max_thinking_length>"), &system);
    }
    if let Some(Entry::User(user)) = entries.first_mut() {
        user.content = messages::join(&system, &user.content);
    }
    for entry in &entries {
        if let Entry::Assistant(message) = entry {
            for call in &message.calls {
                let name = call["name"].as_str().unwrap_or("");
                if declarations
                    .iter()
                    .any(|tool| tool["toolSpecification"]["name"] == name)
                {
                    continue;
                }
                declarations.push(
                    json!({ "toolSpecification": { "name": name, "description": "Tool",
                    "inputSchema": { "json": { "type": "object", "properties": {} } } } }),
                );
            }
        }
    }
    if declarations.len() > 128 {
        return Err(Error::http(400, "Kiro当前及历史工具总数超过上限"));
    }
    let Some(Entry::User(mut current)) = entries.pop() else {
        return Err(Error::http(400, "Kiro缺少当前用户消息"));
    };
    current.tools = declarations;
    let history: Vec<_> = entries
        .into_iter()
        .map(|entry| match entry {
            Entry::User(user) => json!({ "userInputMessage": messages::user_value(&user, model) }),
            Entry::Assistant(message) => {
                json!({ "assistantResponseMessage": messages::assistant_value(&message) })
            }
        })
        .collect();
    let mut state = json!({ "chatTriggerType": "MANUAL", "agentTaskType": "vibe", "conversationId": util::uuid()?,
        "currentMessage": { "userInputMessage": messages::user_value(&current, model) } });
    if !history.is_empty() {
        state["history"] = json!(history);
    }
    let mut out = json!({ "conversationState": state, "agentMode": "vibe" });
    if !profile.is_empty() {
        out["profileArn"] = json!(profile);
    }
    if use_native {
        if let Some(native) = native {
            let mut fields = json!({ native.path: { "effort": level } });
            if native.adaptive {
                fields["thinking"] = json!({ "type": "adaptive" });
            }
            out["additionalModelRequestFields"] = fields;
        }
    }
    if agent2api_wasm_sdk::http::json_exceeds(&out, 14 * 1024 * 1024)? {
        return Err(Error::http(
            413,
            "Kiro对话超过受控传输上限，请压缩历史或图片",
        )
        .with_code("plugin_request_too_large"));
    }
    Ok(Built {
        body: out,
        split_thinking: budget.is_some(),
    })
}
