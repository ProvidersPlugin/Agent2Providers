//! 原生库库仅文本直通，多轮历史拍平并每次创建新会话。
//! 未支持的工具、图片、强制JSON和思考档位明确拒绝，不能接受后丢掉调用语义。

use crate::{endpoints, session::Triple, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

fn text(message: &Value) -> Result<Option<String>> {
    let content = &message["content"];
    if content.is_null() {
        return Ok(None);
    }
    if let Some(text) = content.as_str() {
        return Ok(Some(text.into()));
    }
    let parts = content
        .as_array()
        .ok_or_else(|| Error::http(400, "库库消息内容须为文本或文本块数组"))?;
    let mut values = Vec::new();
    for part in parts {
        if !matches!(
            part["type"].as_str(),
            Some("text" | "input_text" | "output_text")
        ) {
            return Err(Error::http(
                400,
                "库库当前原生链路仅支持文本；图片或附件未被静默丢弃",
            ));
        }
        values.push(
            part["text"]
                .as_str()
                .ok_or_else(|| Error::http(400, "库库文本内容块无效"))?
                .to_string(),
        );
    }
    Ok((!values.is_empty()).then(|| values.join("\n")))
}
pub fn prompt(body: &Value) -> Result<String> {
    for key in ["tools", "functions"] {
        if body
            .get(key)
            .is_some_and(|value| !value.is_null() && !value.as_array().is_some_and(Vec::is_empty))
        {
            return Err(Error::http(
                400,
                "库库当前未实现可靠工具协议，不使用提示词假装工具调用",
            ));
        }
    }
    if body
        .get("function_call")
        .is_some_and(|value| !value.is_null() && value != "none" && value != "auto")
    {
        return Err(Error::http(400, "库库未支持旧式强制工具选择"));
    }
    if body
        .get("tool_choice")
        .is_some_and(|value| !value.is_null() && value != "none" && value != "auto")
    {
        return Err(Error::http(400, "库库未支持强制工具选择"));
    }
    if body
        .get("response_format")
        .or_else(|| body.get("output_format"))
        .is_some_and(|value| !value.is_null() && value["type"] != "text")
    {
        return Err(Error::http(400, "库库当前没有已验证的强制JSON输出协议"));
    }
    if body.get("thinking").is_some_and(|value| !value.is_null())
        || body
            .get("reasoning_effort")
            .is_some_and(|value| !value.is_null())
        || body.get("reasoning").is_some_and(|value| !value.is_null())
    {
        return Err(Error::http(
            400,
            "库库think_mode仅保留原生固定值，不能保证客户端指定思考档位",
        ));
    }
    let messages = body["messages"]
        .as_array()
        .filter(|values| !values.is_empty() && values.len() <= 4096)
        .ok_or_else(|| Error::http(400, "库库消息列表为空或过大"))?;
    let mut out = Vec::new();
    for message in messages {
        let role = message["role"].as_str().unwrap_or("user");
        if message
            .get("function_call")
            .is_some_and(|value| !value.is_null())
        {
            return Err(Error::http(400, "库库当前不支持旧式工具调用历史"));
        }
        if !["system", "developer", "assistant", "user"].contains(&role)
            || message.get("tool_calls").is_some_and(|value| {
                !value.is_null() && !value.as_array().is_some_and(Vec::is_empty)
            })
        {
            return Err(Error::http(
                400,
                "库库当前不支持工具调用历史，未拍平成普通文本",
            ));
        }
        let Some(text) = text(message)?.filter(|text| !text.trim().is_empty()) else {
            continue;
        };
        if messages.len() == 1 {
            out.push(text);
        } else {
            out.push(match role {
                "system" | "developer" => format!("[系统指令] {text}"),
                "assistant" => format!("[助手之前的回复] {text}"),
                _ => text,
            });
        }
    }
    let prompt = out.join("\n\n");
    if prompt.is_empty() {
        return Err(Error::http(400, "库库请求没有可发送文本"));
    }
    if prompt.len() > 512 * 1024 {
        return Err(Error::http(413, "库库历史拍平后过大，请先压缩历史"));
    }
    Ok(prompt)
}
fn data(prompt: &str, model: &str) -> Value {
    let show_text = json!({ "skills": [], "experts": [], "model_name": model, "model_display_name": model,
        "think_mode": endpoints::THINK_MODE, "richInputContent": [{ "type": "text", "text": prompt }], "fileInfo": [] });
    json!({ "project_type": 1, "text": prompt,
        "rich_input_params": [{ "id": "textId", "version": "1.0", "type": "text", "text": prompt, "data": { "content": prompt } }],
        "fsid": [], "quotes": [], "skills": [], "experts": [], "model_name": model, "model_display_name": model,
        "think_mode": endpoints::THINK_MODE, "show_text": show_text.to_string(), "premake_data": {},
        "custom_instructions": "", "memory_sign": true, "skill_dig_sign": true })
}
pub fn create(prompt: &str, model: &str, triple: &Triple) -> Result<Value> {
    let csi = util::uuid()?;
    let client = json!({ "client_session_id": csi, "permission_type": 0, "clienttype": 400,
        "app_id": endpoints::APP_ID, "web": 1, "channel": endpoints::CHANNEL, "version": "1.4.4",
        "bdstoken": triple.bdstoken, "uinfo": triple.uinfo, "uk": triple.uk });
    let mut data = data(prompt, model);
    data["client_added"] = json!(client.to_string());
    Ok(
        json!({ "type": "message", "sub_type": "chat_create", "client_session_id": csi,
        "session_id": "", "uk": "", "cid": 0, "channel": endpoints::CHANNEL_NAME,
        "device_type": 400, "created_at": util::now()?, "msg_type": 1, "sync_type": 0, "sync_id": "",
        "v": util::random_hex(12)?, "data": data }),
    )
}
pub fn generate(prompt: &str, model: &str, session: &str, reply: &str) -> Value {
    let mut body = data(prompt, model);
    body["session_id"] = json!(session);
    body["reply_id"] = json!(reply);
    body
}
