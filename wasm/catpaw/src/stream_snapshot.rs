//! 已证实的 CatPaw 累计消息字段；依据原生 catpaw/openai.rs 的 message_delta /
//! extract_tool_calls / usage_from_response。非前缀覆盖不整体重发；缺字段不补造。

use agent2api_wasm_sdk::{json, Result, Value};
use std::collections::BTreeSet;

use crate::{errors, tools};

const MAX_NODES: usize = 65_536;
const MAX_DEPTH: usize = 64;
const MAX_ARGUMENTS: usize = 256 * 1024;

pub(super) struct Call {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) arguments: String,
}

pub(super) struct Snapshot {
    pub(super) id: Option<String>,
    pub(super) text: String,
    pub(super) reasoning: String,
    pub(super) calls: Vec<Call>,
    pub(super) finished: bool,
    pub(super) raw: Value,
}

pub(super) fn nodes(value: &Value) -> Result<usize> {
    fn walk(value: &Value, depth: usize, count: &mut usize) -> Result<()> {
        *count += 1;
        if *count > MAX_NODES || depth > MAX_DEPTH {
            return Err(errors::protocol(
                "CatPaw 上游事件的节点数或嵌套深度超过限制",
            ));
        }
        match value {
            Value::Array(items) => {
                for item in items {
                    walk(item, depth + 1, count)?;
                }
            }
            Value::Object(object) => {
                for item in object.values() {
                    walk(item, depth + 1, count)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let mut count = 0;
    walk(value, 0, &mut count)?;
    Ok(count)
}

pub(super) fn identifier(value: &Value) -> Result<&str> {
    let id = value
        .as_str()
        .filter(|id| !id.trim().is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
        .ok_or_else(|| errors::protocol("CatPaw 上游标识缺失、过长或包含控制字符"))?;
    Ok(id)
}

impl Snapshot {
    pub(super) fn parse(message: &Value) -> Result<Self> {
        if !message.is_object() || message["type"].as_str() != Some("assistant") {
            return Err(errors::protocol(
                "CatPaw 上游 message 不是已支持的 assistant 消息",
            ));
        }
        let id = message
            .get("messageId")
            .map(identifier)
            .transpose()?
            .map(str::to_string);
        let finished = match message.get("finished") {
            None => false,
            Some(Value::Bool(finished)) => *finished,
            _ => return Err(errors::protocol("CatPaw message.finished 不是布尔值")),
        };
        let blocks = message["content"]
            .as_array()
            .filter(|items| items.len() <= 1024)
            .ok_or_else(|| errors::protocol("CatPaw 上游 content 必须是不超过 1024 项的数组"))?;
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut calls = Vec::new();
        let mut ids = BTreeSet::new();
        for block in blocks {
            match block["type"].as_str() {
                Some("text") => {
                    if block.get("reasoning_content").is_some() {
                        return Err(errors::protocol(
                            "CatPaw 上游思考字段不是已证实的 reasoningContent，未静默降级",
                        ));
                    }
                    let mut present = false;
                    for (field, target) in
                        [("text", &mut text), ("reasoningContent", &mut reasoning)]
                    {
                        if let Some(value) = block.get(field) {
                            present = true;
                            let value = value.as_str().ok_or_else(|| {
                                errors::protocol("CatPaw 累计文本或思考字段不是文本")
                            })?;
                            target.push_str(value);
                        }
                    }
                    if !present {
                        return Err(errors::protocol("CatPaw text 块缺少已支持的正文或思考字段"));
                    }
                }
                Some("tool_use") => {
                    if calls.len() >= 256 {
                        return Err(errors::protocol("CatPaw 上游工具调用超过 256 项"));
                    }
                    let id = identifier(&block["toolCallId"])?;
                    if !ids.insert(id) {
                        return Err(errors::protocol("CatPaw 上游工具调用标识重复"));
                    }
                    let name = block["toolName"]
                        .as_str()
                        .ok_or_else(|| errors::protocol("CatPaw 上游工具调用缺少真实名称"))?;
                    tools::validate_name(name)
                        .map_err(|_| errors::protocol("CatPaw 上游工具名称格式无效"))?;
                    let arguments = block["toolParams"]
                        .as_str()
                        .filter(|args| args.len() <= MAX_ARGUMENTS)
                        .ok_or_else(|| {
                            errors::protocol("CatPaw 上游 toolParams 缺失、不是文本或超过 256 KiB")
                        })?;
                    if finished {
                        let parsed: Value = serde_json::from_str(arguments).map_err(|_| {
                            errors::protocol("CatPaw 已结束消息的工具参数不是完整 JSON，未补空对象")
                        })?;
                        if !parsed.is_object() {
                            return Err(errors::protocol(
                                "CatPaw 已结束消息的工具参数不是 JSON 对象",
                            ));
                        }
                        nodes(&parsed)?;
                        tools::validate_json(&parsed, false).map_err(|_| {
                            errors::protocol("CatPaw 上游工具参数超过结构限制或含不允许的字段")
                        })?;
                    }
                    calls.push(Call {
                        id: id.to_string(),
                        name: name.to_string(),
                        arguments: arguments.to_string(),
                    });
                }
                _ => {
                    return Err(errors::protocol(
                        "CatPaw 上游含尚未迁移的内容块，未静默丢弃",
                    ))
                }
            }
        }
        Ok(Self {
            id,
            text,
            reasoning,
            calls,
            finished,
            raw: message.clone(),
        })
    }

    /// 必须先完整校验一个快照，再返回任何本事件增量；工具首见顺序稳定。
    pub(super) fn delta(
        &self,
        previous: Option<&Self>,
        declared: &[String],
        choice: &tools::ToolChoice,
    ) -> Result<Value> {
        if let Some(previous) = previous {
            if self.id != previous.id {
                return Err(errors::protocol("CatPaw 流内消息身份改变，未合并不同消息"));
            }
            if previous.finished && (!self.finished || self.raw != previous.raw) {
                return Err(errors::protocol(
                    "CatPaw 已结束消息又被修改，未复用旧结束标记",
                ));
            }
            if self.calls.len() < previous.calls.len() {
                return Err(errors::protocol(
                    "CatPaw 累计工具列表回退，未丢弃已输出调用",
                ));
            }
        }
        let mut delta = json!({});
        let text = suffix(&self.text, previous.map(|p| p.text.as_str()).unwrap_or(""))?;
        let reasoning = suffix(
            &self.reasoning,
            previous.map(|p| p.reasoning.as_str()).unwrap_or(""),
        )?;
        if !text.is_empty() {
            delta["content"] = json!(text);
        }
        if !reasoning.is_empty() {
            delta["reasoning_content"] = json!(reasoning);
        }
        let mut calls = Vec::new();
        for (index, call) in self.calls.iter().enumerate() {
            if !declared.contains(&call.name)
                || matches!(choice, tools::ToolChoice::None)
                || matches!(choice, tools::ToolChoice::Function(name) if name != &call.name)
            {
                return Err(errors::protocol(
                    "CatPaw 上游工具不在本次允许的声明中，未过滤成成功",
                ));
            }
            let old = previous.and_then(|p| p.calls.get(index));
            if old.is_some_and(|old| old.id != call.id || old.name != call.name) {
                return Err(errors::protocol(
                    "CatPaw 上游工具标识、名称或顺序改变，未重新编号",
                ));
            }
            let arguments = suffix(
                &call.arguments,
                old.map(|p| p.arguments.as_str()).unwrap_or(""),
            )?;
            if old.is_none() {
                // 参数来自真实 toolParams（包括上游明确给的空串），不伪造占位调用。
                calls.push(json!({"index": index, "id": call.id, "type": "function",
                    "function": {"name": call.name, "arguments": arguments}}));
            } else if !arguments.is_empty() {
                calls.push(json!({"index": index, "function": {"arguments": arguments}}));
            }
        }
        if self.finished
            && self.calls.is_empty()
            && matches!(
                choice,
                tools::ToolChoice::Required | tools::ToolChoice::Function(_)
            )
        {
            return Err(errors::protocol(
                "CatPaw 上游已结束但未满足本次工具调用要求",
            ));
        }
        if !calls.is_empty() {
            delta["tool_calls"] = Value::Array(calls);
        }
        Ok(delta)
    }
}

fn suffix<'a>(current: &'a str, previous: &str) -> Result<&'a str> {
    current.strip_prefix(previous).ok_or_else(|| {
        errors::protocol("CatPaw 累计正文、思考或工具参数发生回退/覆盖，未重复下发全量")
    })
}

/// 只转换完整真实的三个整数计数；不沿用原生的缺值补零、取整或异常钳位。
/// 上游 prompt 是增量、total 是会话累计，沿用已记录的 max(prompt,total-completion) 口径。
pub(super) fn usage(event: &Value) -> Result<Option<Value>> {
    let direct = event.get("usage").filter(|v| !v.is_null());
    let nested = event.pointer("/contextInfo/usage").filter(|v| !v.is_null());
    if direct.is_some() && nested.is_some() && direct != nested {
        return Err(errors::protocol("CatPaw 同一事件的用量来源冲突"));
    }
    let Some(value) = direct.or(nested) else {
        return Ok(None);
    };
    if !value.is_object() {
        return Err(errors::protocol("CatPaw 上游 usage 不是对象"));
    }
    fn count(value: &Value, snake: &str, camel: &str) -> Result<Option<u64>> {
        let mut result = None;
        for field in [snake, camel] {
            if let Some(value) = value.get(field).filter(|v| !v.is_null()) {
                let count = value
                    .as_u64()
                    .filter(|number| *number <= 9_007_199_254_740_991)
                    .ok_or_else(|| errors::protocol("CatPaw 上游用量不是有效的非负整数"))?;
                if result.is_some_and(|previous| previous != count) {
                    return Err(errors::protocol("CatPaw 上游用量同义字段冲突"));
                }
                result = Some(count);
            }
        }
        Ok(result)
    }
    let prompt = count(value, "prompt_tokens", "promptTokens")?;
    let completion = count(value, "completion_tokens", "completionTokens")?;
    let total = count(value, "total_tokens", "totalTokens")?;
    let (Some(prompt), Some(completion), Some(total)) = (prompt, completion, total) else {
        return Ok(None);
    };
    let context_prompt = total
        .checked_sub(completion)
        .ok_or_else(|| errors::protocol("CatPaw 上游总用量小于输出用量，未改写异常计数"))?;
    let prompt = prompt.max(context_prompt);
    let total = prompt
        .checked_add(completion)
        .filter(|value| *value <= 9_007_199_254_740_991)
        .ok_or_else(|| errors::protocol("CatPaw 上游用量合计超过有效整数范围"))?;
    Ok(Some(
        json!({"prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": total}),
    ))
}
