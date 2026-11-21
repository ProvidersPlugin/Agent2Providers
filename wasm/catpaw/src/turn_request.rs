//! Prepared → CatPaw 固定 round/turn 正文，依据原生 build_turn_body / submit_round。
//! 无网络或 ID 生成；新一轮或工具续接模式必须来自本次宿主已核对的真实回执。

use agent2api_wasm_sdk::{json, Result, Value};

use crate::{
    errors,
    messages::Tail,
    models,
    prepare::{self, Correlation, Prepared},
    tools,
    turn_host::{self, Mode, PreparedCall},
};

const SOURCE: &str = "CatX";
const MODE: &str = "CATX_APP";
const TOOL_VERSION: &str = "2.0.2";
const PERMISSION_MODE: &str = "unsafeBypassPermissions";

pub(super) struct Request {
    pub(super) round_json: Option<String>,
    pub(super) turn_json: String,
    pub(super) history_json: String,
    pub(super) request_context_json: String,
}

/// prepare 宿主操作前的纯本地校验；不读凭据/旧状态或生成关联 ID。
pub(super) fn preflight(
    body: &Value,
    selection: &models::Selection,
    client_stream: bool,
) -> Result<(Value, usize)> {
    if !body.is_object() {
        return Err(errors::invalid("CatPaw 对话 body 必须是对象"));
    }
    turn_host::bounded(body, 8 * 1024 * 1024)?;
    let model = selection.request(body)?;
    let tools = tools::prepare(body)?;
    selection.check_tools(!tools.configs.is_empty())?;
    let raw = body["messages"]
        .as_array()
        .filter(|items| !items.is_empty())
        .ok_or_else(|| errors::invalid("CatPaw messages 必须是非空数组"))?;
    let mut message_count = 0usize;
    for message in raw {
        if !message.is_object() {
            return Err(errors::invalid("CatPaw 历史消息必须是对象"));
        }
        match message["role"].as_str() {
            Some("system" | "developer") => {}
            Some("user" | "assistant" | "tool") => {
                message_count += 1;
            }
            _ => return Err(errors::invalid("CatPaw 历史消息角色不受支持")),
        }
        // 本消费者只接受宿主本次ID池，不把客户端自带messageId当可信关联。
        if message.get("id").is_some_and(|id| !id.is_null())
            || message.get("messageId").is_some_and(|id| !id.is_null())
        {
            return Err(errors::invalid(
                "CatPaw 受控新轮次暂不接受客户端消息关联 ID，未以其冒充宿主来源",
            ));
        }
    }
    if message_count == 0 {
        return Err(errors::invalid("CatPaw 指令之外至少需要一条消息"));
    }
    let tail = raw
        .iter()
        .rev()
        .find(|message| !matches!(message["role"].as_str(), Some("system" | "developer")))
        .and_then(|message| message["role"].as_str());
    if !matches!(tail, Some("user" | "tool")) && !(tail == Some("assistant") && !client_stream) {
        return Err(errors::invalid("CatPaw 请求必须以用户消息、完整工具结果或非流式辅助消息结尾"));
    }
    let constraints = json!({"model_type": model.model_type, "model_name": model.display_name,
        "effort": model.effort, "context": model.context, "tool_configs": tools.configs,
        "tool_choice": choice(&tools.choice), "parallel_requested": tools.parallel_requested,
        "client_stream": client_stream});
    turn_host::bounded(&constraints, 3 * 1024 * 1024)?;
    Ok((constraints, message_count))
}

pub(super) fn prepare(
    body: &Value,
    selection: &models::Selection,
    call: &PreparedCall,
) -> Result<Prepared> {
    let prepared = prepare::prepare(
        body,
        selection,
        Correlation {
            conversation_id: &call.conversation_id,
            turn_request_id: &call.turn_request_id,
            message_ids: &call.message_ids,
        },
    )?;
    if prepared.message_ids_used() != call.message_ids.len()
        || prepared.original_message_ids() != call.message_ids.as_slice()
    {
        return Err(errors::protocol(
            "CatPaw 请求消息关联没有完整消费本次宿主 ID 池",
        ));
    }
    Ok(prepared)
}

pub(super) fn build(
    prepared: &Prepared,
    call: &PreparedCall,
    constraints: &Value,
) -> Result<Request> {
    if prepared.correlation() != (call.conversation_id.as_str(), call.turn_request_id.as_str()) {
        return Err(errors::protocol("CatPaw 请求关联与本次宿主分配不一致"));
    }
    match call.mode {
        Mode::NewRound if call.previous.is_none() && call.session_revision == 0 => {},
        Mode::ToolContinuation if call.previous.is_some() && call.session_revision > 0 => {},
        _ => return Err(errors::protocol("CatPaw 模式与原宿主会话回执不一致")),
    }
    let client_stream = constraints["client_stream"]
        .as_bool()
        .ok_or_else(|| errors::protocol("CatPaw 新轮次缺少真实请求类型"))?;
    let tail_valid = match call.mode {
        Mode::NewRound => matches!(prepared.tail(), Tail::User)
            || matches!(prepared.tail(), Tail::Assistant) && !client_stream,
        Mode::ToolContinuation => matches!(prepared.tail(), Tail::ToolResults(_)),
    };
    if !tail_valid { return Err(errors::invalid("CatPaw 本次消息结尾与宿主已批准模式不一致")); }
    let context = json!({"system_prompt": prepared.system_prompt(), "rules_message": prepared.rules_message()});
    let history = history(prepared.messages())?;
    let message = prepared
        .messages()
        .last()
        .ok_or_else(|| errors::invalid("CatPaw 缺少本次最后消息"))?;
    let model = prepared.model();
    let tools = prepared.tools();
    let mut turn = json!({"conversationId": call.conversation_id, "turnRequestId": call.turn_request_id,
        "source": SOURCE, "action": "turn", "message": message, "modelType": model.model_type,
        "mode": MODE, "permissionMode": PERMISSION_MODE, "toolVersion": TOOL_VERSION,
        "toolConfigs": tools.configs, "availableTools": tools.available});
    directives(&mut turn, prepared);
    let mut round = json!({"conversationId": call.conversation_id, "source": SOURCE,
        "messages": history, "modelType": model.model_type, "mode": MODE,
        "permissionMode": PERMISSION_MODE, "toolVersion": TOOL_VERSION});
    directives(&mut round, prepared);
    let mut params = json!({});
    if let Some(effort) = &model.effort {
        params["effort"] = json!(effort);
    }
    if let Some(context) = &model.context {
        params["context"] = json!(context);
    }
    if params.as_object().is_some_and(|fields| !fields.is_empty()) {
        round["requestContext"] = json!({"modelParams": {"declarativeParams": params}});
    }
    let request = Request {
        round_json: if call.mode == Mode::NewRound { Some(turn_host::encode(&round)?) } else { None },
        turn_json: turn_host::encode(&turn)?,
        history_json: turn_host::encode(&history)?,
        request_context_json: turn_host::encode(&context)?,
    };
    let total = request
        .round_json
        .as_ref()
        .map(String::len)
        .unwrap_or(0)
        .saturating_add(request.turn_json.len())
        .saturating_add(request.history_json.len())
        .saturating_add(request.request_context_json.len());
    if total > turn_host::MAX_MATERIAL {
        return Err(errors::invalid(
            "CatPaw 本次完整 round/turn/历史材料超过 16 MiB",
        ));
    }
    // start 还会计外层JSON转义后的确切16MiB，不能只按未转义正文放行。
    Ok(request)
}

fn directives(body: &mut Value, prepared: &Prepared) {
    if let Some(prompt) = prepared.system_prompt() {
        body["systemPromptContext"] = json!({"systemPromptOverride": prompt});
    }
    if let Some(rules) = prepared.rules_message() {
        body["rulesMessage"] = json!(rules);
    }
}

fn history(messages: &[Value]) -> Result<Value> {
    for message in messages {
        turn_host::exact(message, &["type", "messageId", "content", "finished"])?;
        if !message["type"]
            .as_str()
            .is_some_and(|kind| matches!(kind, "user" | "assistant" | "tool"))
            || !message["content"].is_array()
            || message["finished"].as_bool().is_none()
        {
            return Err(errors::protocol("CatPaw 规范历史结构异常"));
        }
    }
    // 同一数组用于history_json与round.messages；turn.message直接取相同末项。
    Ok(Value::Array(messages.to_vec()))
}

fn choice(choice: &tools::ToolChoice) -> Value {
    match choice {
        tools::ToolChoice::Auto => json!("auto"),
        tools::ToolChoice::None => json!("none"),
        tools::ToolChoice::Required => json!("required"),
        tools::ToolChoice::Function(name) => {
            json!({"type": "function", "function": {"name": name}})
        }
    }
}
