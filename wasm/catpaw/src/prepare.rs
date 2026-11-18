//! 供后续受控执行器使用的纯准备入口：规范 Chat body + 本家目录选择 + 外部关联 ID。
//! 此处不读宿主/账号/状态、不发请求、不恢复会话；返回值没有任何创建/运行/完成回执。
//! Tail::ToolResults 只是本次历史已配对的候选材料，不能据此选择续接或新 round。

use agent2api_wasm_sdk::{Result, Value};
use std::io::{self, Write};

use crate::{blocks, errors, messages, models, tools};

const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_NODES: usize = 65_536;
const MAX_DEPTH: usize = 64;
const MAX_DIRECTIVE_BYTES: usize = 1024 * 1024;

/// 由未来宿主入口单独传入；文本格式正确不证明其可信，更不证明对象已在远端创建。
/// 不从 Chat body 的 conversationId / turnRequestId 取值，不提供反序列化入口。
pub(super) struct Correlation<'a> {
    pub(super) conversation_id: &'a str,
    pub(super) turn_request_id: &'a str,
    /// 仅补缺失的历史 message.id，按非指令消息原始顺序消费；不包含合成消息。
    pub(super) message_ids: &'a [String],
}

pub(super) struct Prepared {
    conversation_id: String,
    turn_request_id: String,
    history: messages::Normalized,
    model: models::RequestModel,
    tools: tools::PreparedTools,
}

impl Prepared {
    pub(super) fn correlation(&self) -> (&str, &str) { (&self.conversation_id, &self.turn_request_id) }
    pub(super) fn messages(&self) -> &[Value] { &self.history.messages }
    pub(super) fn system_prompt(&self) -> Option<&str> { self.history.system_prompt.as_deref() }
    pub(super) fn rules_message(&self) -> Option<&str> { self.history.rules_message.as_deref() }
    pub(super) fn tail(&self) -> &messages::Tail { &self.history.tail }
    pub(super) fn message_ids_used(&self) -> usize { self.history.supplied_ids_used }
    pub(super) fn original_message_ids(&self) -> &[String] { &self.history.original_message_ids }
    pub(super) fn model(&self) -> &models::RequestModel { &self.model }
    pub(super) fn tools(&self) -> &tools::PreparedTools { &self.tools }
}

/// body 是现有 conversation 参数里的 params["body"]，不是另造的演示请求。
/// selection 由 models::select(本次 catalog::list() 结果, &body["model"]) 产生。
/// client_stream、账号来源、安装代次与发送/续接许可均留给真实宿主执行器验证。
pub(super) fn prepare(body: &Value, selection: &models::Selection, ids: Correlation<'_>) -> Result<Prepared> {
    if !body.is_object() { return Err(errors::invalid("CatPaw Chat 请求体必须是对象")); }
    input_budget(body)?;
    validate_correlation(&ids)?;
    unsupported_fields(body)?;
    let model = selection.request(body)?;
    let tools = tools::prepare(body)?;
    selection.check_tools(!tools.configs.is_empty())?;
    let raw = body["messages"].as_array()
        .ok_or_else(|| errors::invalid("CatPaw messages 必须是非空数组"))?;
    // 不把图片混在历史或工具返回结构内悄悄变成文本。
    for message in raw {
        if message["role"] != "user" { blocks::reject_images(&message["content"])?; }
        if message["content"].as_array().is_some_and(|parts| parts.iter().any(|part| part["type"] == "image_url")) {
            selection.check_images()?;
        }
    }
    let history = messages::normalize(raw, ids.message_ids)?;
    if history.original_message_ids.iter().any(|id|
        id == ids.conversation_id || id == ids.turn_request_id) {
        return Err(errors::invalid("CatPaw 历史 messageId 不能冒充本次 conversation 或 turn 关联 ID"));
    }
    for text in [&history.system_prompt, &history.rules_message].into_iter().flatten() {
        if text.len() > MAX_DIRECTIVE_BYTES {
            return Err(errors::invalid("CatPaw 系统提示或规则超过 1 MiB，未截断发送"));
        }
    }
    let mut total = ids.conversation_id.len() + ids.turn_request_id.len() + model.display_name.len();
    for text in [model.effort.as_deref(), model.context.as_deref(), history.system_prompt.as_deref(),
        history.rules_message.as_deref()].into_iter().flatten() {
        add_bytes(&mut total, json_string_bytes(text)?)?;
    }
    for message in &history.messages { add_bytes(&mut total, json_bytes(message, MAX_OUTPUT_BYTES)?)?; }
    for id in &history.original_message_ids { add_bytes(&mut total, json_string_bytes(id)?)?; }
    for tool in &tools.configs { add_bytes(&mut total, json_bytes(tool, MAX_OUTPUT_BYTES)?)?; }
    for name in &tools.available { add_bytes(&mut total, json_string_bytes(name)?)?; }
    if let tools::ToolChoice::Function(name) = &tools.choice { add_bytes(&mut total, json_string_bytes(name)?)?; }
    if let messages::Tail::ToolResults(references) = &history.tail {
        for reference in references {
            add_bytes(&mut total, json_string_bytes(&reference.id)?)?;
            add_bytes(&mut total, json_string_bytes(&reference.name)?)?;
        }
    }
    // 预留容器/字段名称，不声称这是最终 HTTP 正文大小；执行器仍须校验确切请求。
    add_bytes(&mut total, 32 * 1024)?;
    Ok(Prepared { conversation_id: ids.conversation_id.to_string(), turn_request_id: ids.turn_request_id.to_string(),
        history, model, tools })
}

fn validate_correlation(ids: &Correlation<'_>) -> Result<()> {
    for id in [ids.conversation_id, ids.turn_request_id] {
        if id.is_empty() || id.len() > 256
            || !id.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte)) {
            return Err(errors::invalid("CatPaw 本次关联 ID 缺失或格式无效，未自行生成"));
        }
    }
    if ids.conversation_id == ids.turn_request_id {
        return Err(errors::invalid("CatPaw conversation 与 turn 关联 ID 不能相同"));
    }
    Ok(())
}

fn unsupported_fields(body: &Value) -> Result<()> {
    // 不让客户端越过宿主带入控制字段；普通扩展字段不会被复制到上游材料。
    for key in ["conversationId", "turnRequestId", "modelType", "permissionMode", "source", "mode",
        "toolVersion", "toolConfigs", "availableTools", "repositories"] {
        if body.get(key).is_some_and(|value| !value.is_null()) {
            return Err(errors::invalid("CatPaw Chat 请求不能携带上游会话或执行控制字段"));
        }
    }
    for key in ["functions", "function_call", "toolCalls"] {
        if body.get(key).is_some_and(|value| !value.is_null()) {
            return Err(errors::invalid("CatPaw 请使用规范 tools/tool_choice 字段，未忽略旧工具协议"));
        }
    }
    if body.get("n").is_some_and(|value| !value.is_null() && value.as_u64() != Some(1)) {
        return Err(errors::invalid("CatPaw 一次请求只支持一个回答（n=1）"));
    }
    if let Some(format) = body.get("response_format").filter(|value| !value.is_null()) {
        if format["type"].as_str() != Some("text") {
            return Err(errors::invalid("CatPaw 当前转换尚未接入结构化输出约束，未忽略 response_format"));
        }
    }
    // 不读 body.stream；它可能已被宿主归一化，不能拿来选择长会话或辅助请求。
    Ok(())
}

fn input_budget(value: &Value) -> Result<()> {
    fn walk(value: &Value, depth: usize, nodes: &mut usize) -> Result<()> {
        *nodes += 1;
        if *nodes > MAX_NODES || depth > MAX_DEPTH {
            return Err(errors::invalid("CatPaw 请求 JSON 的节点数或嵌套深度超过限制"));
        }
        match value {
            Value::Array(items) => { for item in items { walk(item, depth + 1, nodes)?; } },
            Value::Object(object) => { for item in object.values() { walk(item, depth + 1, nodes)?; } },
            _ => {},
        }
        Ok(())
    }
    let mut nodes = 0;
    walk(value, 0, &mut nodes)?;
    json_bytes(value, MAX_INPUT_BYTES)?;
    Ok(())
}

fn add_bytes(total: &mut usize, bytes: usize) -> Result<()> {
    *total = total.checked_add(bytes).filter(|total| *total <= MAX_OUTPUT_BYTES)
        .ok_or_else(|| errors::invalid("CatPaw 待提交材料超过 8 MiB，未截断发送"))?;
    Ok(())
}

struct Counter { bytes: usize, limit: usize }
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.checked_add(bytes.len()).filter(|size| *size <= self.limit)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "material limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

/// 流式计数而不是 value.to_string() 后再检查，避免校验时额外复制整份大 JSON。
pub(super) fn json_bytes(value: &Value, limit: usize) -> Result<usize> {
    let mut counter = Counter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, value)
        .map_err(|_| errors::invalid("CatPaw JSON 材料超过大小限制或无法编码"))?;
    Ok(counter.bytes)
}

fn json_string_bytes(value: &str) -> Result<usize> {
    let mut counter = Counter { bytes: 0, limit: MAX_OUTPUT_BYTES };
    serde_json::to_writer(&mut counter, value)
        .map_err(|_| errors::invalid("CatPaw 文本材料超过大小限制或无法编码"))?;
    Ok(counter.bytes)
}
