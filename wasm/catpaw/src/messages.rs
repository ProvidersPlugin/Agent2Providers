//! 规范 Chat 历史 → CatPaw 消息块。只处理本次输入，不查询或恢复上游会话。
//! 必须完整配对真实 tool 结果；不合成“被中断”、不补“继续”，末尾工具结果只标作候选。

use agent2api_wasm_sdk::{json, Result, Value};
use std::collections::{BTreeMap, BTreeSet};

use crate::{blocks, errors, tools};

const MAX_MERGED_BLOCKS: usize = 1024;
const MAX_HISTORY_CALLS: usize = 4096;

/// 来自本次历史的声明关系，不表示已通过宿主的原账号/原轮次续接校验。
pub(super) struct ToolReference {
    pub(super) id: String,
    pub(super) name: String,
}

pub(super) enum Tail {
    User,
    /// 辅助请求可有此结尾；是否允许执行要由真实 client_stream 上下文决定。
    Assistant,
    /// 仅为候选，不可据此跳过 round 或向任何已有 conversation 提交。
    ToolResults(Vec<ToolReference>),
}

pub(super) struct Normalized {
    pub(super) messages: Vec<Value>,
    pub(super) system_prompt: Option<String>,
    pub(super) rules_message: Option<String>,
    pub(super) tail: Tail,
    pub(super) supplied_ids_used: usize,
    /// 每个原始非指令消息使用的 ID（合并掉的消息也保留），便于调用方复核来源。
    pub(super) original_message_ids: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role { User, Assistant, Tool }

impl Role {
    fn wire(self) -> &'static str {
        match self { Self::User => "user", Self::Assistant => "assistant", Self::Tool => "tool" }
    }
}

struct Message {
    role: Role,
    id: String,
    content: Vec<Value>,
    /// 仅为入站历史消息的完成标志，不是远端执行完成事实。
    finished: bool,
    calls: Vec<ToolReference>,
}

struct IdPool<'a> {
    supplied: &'a [String],
    used: usize,
    seen: BTreeSet<String>,
}

impl<'a> IdPool<'a> {
    fn new(supplied: &'a [String]) -> Result<Self> {
        let mut supplied_ids = BTreeSet::new();
        for id in supplied {
            check_message_id(id)?;
            if !supplied_ids.insert(id) { return Err(errors::invalid("CatPaw 提供的消息关联 ID 有重复")); }
        }
        Ok(Self { supplied, used: 0, seen: BTreeSet::new() })
    }

    fn take(&mut self, message: &Value) -> Result<String> {
        let explicit = match message.get("id") {
            None | Some(Value::Null) => None,
            Some(Value::String(id)) if id.trim().is_empty() => None,
            Some(Value::String(id)) => Some(id.as_str()),
            _ => return Err(errors::invalid("CatPaw 历史消息 id 必须是文本")),
        };
        let id = match explicit {
            Some(id) => id,
            None => {
                let id = self.supplied.get(self.used)
                    .ok_or_else(|| errors::invalid("CatPaw 缺少本次消息关联 ID，未自行生成或发送请求"))?;
                self.used += 1;
                id.as_str()
            },
        };
        check_message_id(id)?;
        if !self.seen.insert(id.to_string()) {
            return Err(errors::invalid("CatPaw 历史消息使用了重复的 messageId"));
        }
        Ok(id.to_string())
    }
}

fn check_message_id(id: &str) -> Result<()> {
    if id.trim().is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
        return Err(errors::invalid("CatPaw 消息关联 ID 为空、过长或含控制字符"));
    }
    Ok(())
}

/// 必须先经过 prepare 的全体输入预算检查；缺失 id 按非指令消息原始顺序消耗 supplied_ids。
pub(super) fn normalize(raw: &[Value], supplied_ids: &[String]) -> Result<Normalized> {
    if raw.is_empty() {
        return Err(errors::invalid("CatPaw messages 必须是非空数组"));
    }
    let mut ids = IdPool::new(supplied_ids)?;
    let mut stage: Vec<Message> = Vec::with_capacity(raw.len());
    let mut system = Vec::new();
    let mut rules = Vec::new();
    let mut original_message_ids = Vec::new();
    for (index, message) in raw.iter().enumerate() {
        if !message.is_object() {
            return Err(errors::invalid(&format!("messages[{index}] 必须是对象")));
        }
        let role = message["role"].as_str()
            .ok_or_else(|| errors::invalid(&format!("messages[{index}] 缺少有效角色")))?;
        if matches!(role, "system" | "developer") {
            let text = directive(message)?;
            if role == "system" { system.push(text); } else { rules.push(text); }
            continue;
        }
        if !matches!(role, "user" | "assistant" | "tool") {
            return Err(errors::invalid(&format!("messages[{index}] 不是支持的历史角色")));
        }
        let id = ids.take(message)?;
        original_message_ids.push(id.clone());
        let next = match role {
            "user" => user(message, id)?,
            "assistant" => assistant(message, id)?,
            "tool" => tool(message, id)?,
            _ => return Err(errors::invalid(&format!("messages[{index}] 不是支持的历史角色"))),
        };
        merge(&mut stage, next)?;
    }
    if stage.is_empty() {
        return Err(errors::invalid("CatPaw system/developer 之外至少需要一条历史消息"));
    }
    let tail = pair(&mut stage)?;
    let messages = stage.into_iter().map(|message| json!({
        "type": message.role.wire(), "messageId": message.id,
        "content": message.content, "finished": message.finished,
    })).collect();
    Ok(Normalized { messages, system_prompt: join(system), rules_message: join(rules),
        tail, supplied_ids_used: ids.used, original_message_ids })
}

fn directive(message: &Value) -> Result<String> {
    forbid_fields(message, &["tool_calls", "toolCalls", "tool_call_id", "toolCallId", "function_call"])?;
    let content = blocks::content(message.get("content"))?;
    let text = content.iter().filter_map(|block| block["text"].as_str()).collect::<Vec<_>>().join("\n");
    if text.trim().is_empty() {
        return Err(errors::invalid("CatPaw system/developer 必须包含非空文本"));
    }
    Ok(text)
}

fn user(message: &Value, id: String) -> Result<Message> {
    forbid_fields(message, &["tool_calls", "toolCalls", "tool_call_id", "toolCallId", "function_call"])?;
    let content = blocks::content(message.get("content"))?;
    if content.is_empty() { return Err(errors::invalid("CatPaw user 消息内容不能为空")); }
    Ok(Message { role: Role::User, id, content, finished: finished(message)?, calls: Vec::new() })
}

fn assistant(message: &Value, id: String) -> Result<Message> {
    forbid_fields(message, &["tool_call_id", "toolCallId"])?;
    let mut content = blocks::content(message.get("content"))?;
    let reasoning = message.get("reasoning_content").filter(|value| !value.is_null())
        .or_else(|| message.get("reasoningContent").filter(|value| !value.is_null()));
    if let Some(reasoning) = reasoning {
        let text = reasoning.as_str().ok_or_else(|| errors::invalid("CatPaw assistant 思考内容必须是文本"))?;
        if !text.is_empty() {
            if let Some(block) = content.first_mut() { block["reasoningContent"] = json!(text); }
            else { content.push(json!({ "type": "text", "text": "", "reasoningContent": text })); }
        }
    }
    let mut calls = Vec::new();
    for call in blocks::tool_calls(message)? {
        content.push(json!({ "type": "tool_use", "toolCallId": call.id,
            "toolName": call.name, "toolParams": call.arguments }));
        calls.push(ToolReference { id: call.id, name: call.name });
    }
    if content.len() > MAX_MERGED_BLOCKS {
        return Err(errors::invalid("CatPaw assistant 内容块超过限制"));
    }
    Ok(Message { role: Role::Assistant, id, content, finished: finished(message)?, calls })
}

fn tool(message: &Value, id: String) -> Result<Message> {
    forbid_fields(message, &["tool_calls", "toolCalls", "toolCallId", "function_call"])?;
    let call = message["tool_call_id"].as_str()
        .ok_or_else(|| errors::invalid("CatPaw tool 消息缺少 tool_call_id"))?;
    tools::validate_call_id(call)?;
    let mut block = json!({ "type": "tool_result", "toolCallId": call,
        "toolResult": blocks::tool_result(message)? });
    match message.get("name") {
        None | Some(Value::Null) => {},
        Some(Value::String(name)) if name.trim().is_empty() => {},
        Some(Value::String(name)) => {
            tools::validate_name(name.trim())?;
            block["toolName"] = json!(name.trim());
        },
        _ => return Err(errors::invalid("CatPaw tool.name 必须是文本")),
    }
    Ok(Message { role: Role::Tool, id, content: vec![block],
        finished: finished(message)?, calls: Vec::new() })
}

fn finished(message: &Value) -> Result<bool> {
    match message.get("finished") {
        None => Ok(true),
        Some(Value::Bool(value)) => Ok(*value),
        _ => Err(errors::invalid("CatPaw 历史消息 finished 必须是布尔值")),
    }
}

fn forbid_fields(message: &Value, fields: &[&str]) -> Result<()> {
    if fields.iter().any(|field| message.get(*field).is_some_and(|value| !value.is_null())) {
        return Err(errors::invalid("CatPaw 历史消息携带了不属于该角色的工具字段"));
    }
    Ok(())
}

fn merge(stage: &mut Vec<Message>, next: Message) -> Result<()> {
    let mergeable = stage.last().is_some_and(|previous| previous.role == next.role
        && (next.role == Role::Tool || next.role == Role::Assistant
            && (previous.calls.is_empty() || next.calls.is_empty())));
    if !mergeable { stage.push(next); return Ok(()); }
    let previous = stage.last_mut().ok_or_else(|| errors::invalid("CatPaw 历史消息合并状态无效"))?;
    if previous.content.len().saturating_add(next.content.len()) > MAX_MERGED_BLOCKS {
        return Err(errors::invalid("CatPaw 合并后的历史内容块超过 1024 项"));
    }
    if previous.finished != next.finished {
        return Err(errors::invalid("CatPaw 相邻消息的 finished 不一致，未丢弃历史状态进行合并"));
    }
    previous.content.extend(next.content);
    previous.calls.extend(next.calls);
    // 保留首条 id 和 finished，按旧协议只合并内容，不制造新的消息身份。
    Ok(())
}

fn pair(messages: &mut [Message]) -> Result<Tail> {
    let mut pending = BTreeMap::<String, String>::new();
    let mut known = BTreeSet::new();
    let mut tail = Tail::User;
    for message in messages {
        match message.role {
            Role::Assistant => {
                require_resolved(&pending)?;
                for call in &message.calls {
                    if !known.insert(call.id.clone()) {
                        return Err(errors::invalid("CatPaw 历史出现重复的 tool_call_id"));
                    }
                    if known.len() > MAX_HISTORY_CALLS {
                        return Err(errors::invalid("CatPaw 历史工具调用超过 4096 项"));
                    }
                    pending.insert(call.id.clone(), call.name.clone());
                }
                tail = Tail::Assistant;
            },
            Role::Tool => {
                let mut matched = Vec::with_capacity(message.content.len());
                for block in &mut message.content {
                    let id = block["toolCallId"].as_str()
                        .ok_or_else(|| errors::invalid("CatPaw 工具结果缺少配对标识"))?.to_string();
                    let name = pending.remove(&id)
                        .ok_or_else(|| errors::invalid("CatPaw 工具结果没有对应待响应调用，或被重复提交"))?;
                    if block["toolName"].as_str().is_some_and(|actual| actual != name) {
                        return Err(errors::invalid("CatPaw 工具结果名称与 assistant 的工具调用不一致"));
                    }
                    block["toolName"] = json!(name);
                    matched.push(ToolReference { id, name });
                }
                // 连续 tool 已合并；完整配对必须在离开这一组时成立。
                require_resolved(&pending)?;
                tail = Tail::ToolResults(matched);
            },
            Role::User => { require_resolved(&pending)?; tail = Tail::User; },
        }
    }
    require_resolved(&pending)?;
    Ok(tail)
}

fn require_resolved(pending: &BTreeMap<String, String>) -> Result<()> {
    if pending.is_empty() { Ok(()) }
    else { Err(errors::invalid("CatPaw 历史工具调用缺少真实结果，请提供全部对应 tool 消息；未合成中断结果")) }
}

fn join(parts: Vec<String>) -> Option<String> {
    if parts.is_empty() { None } else { Some(parts.join("\n\n")) }
}
