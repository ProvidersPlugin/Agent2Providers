//! AutoClaw 国际版原生 Chat SSE；端点、模型路由与请求指纹不借用国内通道。
//! 按 plugins#437 两地区修复规范化历史载体，保留工具、图片、思考文本和轮次。
//! 新国际客户端使用自己的系统首句；不删除原系统提示，也不触发奖励来绕过拒绝。
use super::{account, config, endpoints, errors, headers, models, prompt, refresh, util};
use agent2api_wasm_sdk::{self as sdk, http, json, sse::SseDecoder, Error, Result, Value};
use std::collections::BTreeMap;

struct Output {
    decoder: SseDecoder,
    started: bool,
    done: bool,
    pending: Vec<Value>,
    pending_bytes: usize,
    choices: BTreeMap<u64, bool>,
    strip: bool,
}
impl Output {
    fn new(strip: bool) -> Self {
        Self {
            decoder: SseDecoder::default(),
            started: false,
            done: false,
            pending: Vec::new(),
            pending_bytes: 0,
            choices: BTreeMap::new(),
            strip,
        }
    }
    fn push(&mut self, bytes: &[u8], model: &str) -> Result<()> {
        for text in self.decoder.push(bytes)? {
            if self.done {
                break;
            }
            if text.trim() == "[DONE]" {
                if self.choices.is_empty() || !self.choices.values().all(|finished| *finished) {
                    return Err(Error::http(
                        502,
                        "AutoClaw 未返回完整结束原因，未伪造成功完成",
                    ));
                }
                self.flush()?;
                sdk::stream_bytes(b"data: [DONE]\n\n")?;
                self.done = true;
                break;
            }
            let mut value: Value = serde_json::from_str(&text)
                .map_err(|_| Error::http(502, "AutoClaw SSE 事件格式无效"))?;
            if errors::has_error(&value) {
                let status = value["error"]["status"]
                    .as_u64()
                    .or_else(|| value["status"].as_u64())
                    .and_then(|value| u16::try_from(value).ok())
                    .unwrap_or(502);
                return Err(errors::failure(status, &value));
            }
            let choices = value["choices"]
                .as_array()
                .ok_or_else(|| Error::http(502, "AutoClaw SSE 事件缺少 choices"))?;
            let mut productive = false;
            for (at, choice) in choices.iter().enumerate() {
                let index = choice["index"].as_u64().unwrap_or(at as u64);
                if matches!(
                    choice["finish_reason"].as_str(),
                    Some("content_filter" | "content_policy_violation")
                ) {
                    return Err(errors::failure(200, &json!({ "code": "content_filter" })));
                }
                let finished = choice["finish_reason"]
                    .as_str()
                    .is_some_and(|reason| !reason.is_empty());
                *self.choices.entry(index).or_insert(false) |= finished;
                productive |= finished
                    || choice["delta"].as_object().is_some_and(|delta| {
                        delta
                            .iter()
                            .any(|(key, value)| key != "role" && util::truthy(value))
                    });
            }
            if self.strip && newline_only(&value) {
                continue;
            }
            value["model"] = json!(model);
            if self.started {
                sdk::stream_event(&value)?;
            } else {
                self.pending_bytes = self.pending_bytes.saturating_add(text.len());
                if self.pending.len() >= 128 || self.pending_bytes > 1024 * 1024 {
                    return Err(Error::http(502, "AutoClaw 仅返回控制帧，没有有效模型内容"));
                }
                self.pending.push(value);
                if productive {
                    self.flush()?;
                }
            }
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        if !self.started {
            sdk::stream_head(200)?;
            self.started = true;
        }
        for event in self.pending.drain(..) {
            sdk::stream_event(&event)?;
        }
        self.pending_bytes = 0;
        Ok(())
    }
}
fn newline_only(value: &Value) -> bool {
    if value.get("error").is_some() || value.get("usage").is_some_and(|value| !value.is_null()) {
        return false;
    }
    let Some(choices) = value["choices"]
        .as_array()
        .filter(|items| !items.is_empty())
    else {
        return false;
    };
    choices.iter().all(|choice| {
        if util::truthy(&choice["finish_reason"]) {
            return false;
        }
        let Some(delta) = choice["delta"].as_object() else {
            return false;
        };
        delta
            .get("content")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty() && text.chars().all(|ch| ch == '\r' || ch == '\n'))
            && !delta
                .iter()
                .any(|(key, value)| key != "content" && key != "role" && util::truthy(value))
    })
}

fn normalize_history(body: &mut Value) -> Result<()> {
    let messages = body["messages"]
        .as_array_mut()
        .filter(|messages| !messages.is_empty())
        .ok_or_else(|| Error::http(400, "AutoClaw 国际版请求必须包含非空消息历史"))?;
    for message in messages {
        let fields = message
            .as_object_mut()
            .ok_or_else(|| Error::http(400, "AutoClaw 历史消息必须是对象"))?;
        if fields
            .get("role")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(Error::http(400, "AutoClaw 历史消息缺少角色"));
        }
        if fields.get("content").is_none_or(Value::is_null) {
            fields.insert("content".into(), json!(""));
        }
        if fields
            .get("content")
            .is_none_or(|content| !content.is_string() && !content.is_array())
        {
            return Err(Error::http(400, "AutoClaw 消息正文必须是文本或多模态数组"));
        }
        // 国际与国内网关都不能解析此历史字段；改换载体而不丢掉原思考内容。
        if let Some(reasoning) = fields
            .remove("reasoning_content")
            .filter(|value| !value.is_null())
        {
            let text = reasoning
                .as_str()
                .ok_or_else(|| Error::http(400, "AutoClaw 历史思考必须是文本"))?;
            if !text.is_empty() {
                let prefix = format!("[Previous reasoning]\n{text}\n[/Previous reasoning]\n\n");
                match fields.get_mut("content") {
                    Some(Value::String(content)) => content.insert_str(0, &prefix),
                    Some(Value::Array(parts)) => {
                        parts.insert(0, json!({ "type": "text", "text": prefix }))
                    }
                    _ => return Err(Error::http(400, "AutoClaw 历史思考没有有效正文载体")),
                }
            }
        }
        if let Some(calls) = fields
            .get_mut("tool_calls")
            .filter(|value| !value.is_null())
        {
            let calls = calls
                .as_array_mut()
                .ok_or_else(|| Error::http(400, "AutoClaw 历史工具调用必须是数组"))?;
            for call in calls {
                let function = call
                    .get_mut("function")
                    .and_then(Value::as_object_mut)
                    .ok_or_else(|| Error::http(400, "AutoClaw 历史工具调用缺少工具说明"))?;
                if function
                    .get("name")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    return Err(Error::http(400, "AutoClaw 历史工具调用缺少工具名称"));
                }
                let args = function.entry("arguments").or_insert(Value::Null);
                if !args.is_string() {
                    *args = json!(if args.is_null() {
                        "{}".to_string()
                    } else {
                        args.to_string()
                    });
                }
            }
        }
    }
    if let Some(tools) = body.get("tools") {
        if tools.is_null() || tools.as_array().is_some_and(Vec::is_empty) {
            if let Some(fields) = body.as_object_mut() {
                fields.remove("tools");
                fields.remove("tool_choice");
            }
        } else if !tools.is_array() {
            return Err(Error::http(400, "AutoClaw 工具声明必须是数组"));
        }
    }
    Ok(())
}

fn session_id(params: &Value, snapshot: &account::Snapshot) -> Result<String> {
    let session = params["session"].as_str().unwrap_or("");
    let first = params["body"]["messages"].as_array().and_then(|messages| {
        messages
            .iter()
            .find(|message| !matches!(message["role"].as_str(), Some("system" | "developer")))
    });
    if session.is_empty() && first.is_none() {
        return util::uuid();
    }
    // 关联而非授权：优先原宿主会话标识，缺失时由首个非系统消息保持多轮稳定。
    // 账号/地区/身份一起摘要，不把内容、下游凭据或标识原文放进上游请求头。
    let key = if session.is_empty() {
        json!([first])
    } else {
        json!([session])
    };
    let hash = util::hash(
        &json!([
            endpoints::PROVIDER,
            snapshot.record["id"],
            account::user(&snapshot.secret),
            key
        ])
        .to_string(),
    );
    Ok(format!(
        "{}-{}-4{}-8{}-{}",
        &hash[..8],
        &hash[8..12],
        &hash[13..16],
        &hash[17..20],
        &hash[20..32]
    ))
}

pub fn forward(params: &Value) -> Result<()> {
    // 先验证与整理纯请求字段，不为尚无有效请求的输入消耗一次性刷新凭据。
    let mut body = params["body"].clone();
    if !body.is_object() {
        return Err(Error::http(400, "AutoClaw 请求体必须是对象"));
    }
    normalize_history(&mut body)?;
    let route = models::route(body["model"].as_str().unwrap_or(""))?;
    body["model"] = json!(route.route);
    body["stream"] = json!(true);
    if let Some(fields) = body.as_object_mut() {
        // 国际客户端与公开双架构回执不发送这两个不被接受的预算字段。
        fields.remove("max_tokens");
        fields.remove("max_completion_tokens");
    }
    prompt::normalize(&mut body);
    if let Some(object) = body.as_object_mut() {
        object.remove("metadata");
    }
    let strip = config::strip_newlines()?;
    let token = refresh::ensure()?;
    let snapshot = account::read()?;
    if account::access(&snapshot.secret) != token {
        return Err(Error::http(409, "AutoClaw 账号凭据已变化，请重试"));
    }
    let session = session_id(params, &snapshot)?;
    if !account::same(&snapshot, &account::read()?) {
        return Err(Error::http(
            409,
            "AutoClaw 账号已变化，未发送旧账号模型请求",
        ));
    }
    let mut response = http::open(
        "POST",
        &format!("{}{}", endpoints::ORIGIN, endpoints::CHAT),
        headers::chat(&token, &route.route, &session, &params["headers"])?,
        Some(&body),
    )?;
    let content_type = response.headers["content-type"]
        .as_str()
        .unwrap_or("")
        .to_ascii_lowercase();
    if !(200..300).contains(&response.status) || content_type.contains("application/json") {
        let status = response.status;
        let bytes = response.bytes(1024 * 1024)?;
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        return Err(errors::failure(status, &value));
    }
    if content_type.contains("text/html") {
        return Err(Error::http(502, "AutoClaw 返回网页而不是模型结果"));
    }
    let mut output = Output::new(strip);
    let result = (|| -> Result<()> {
        while let Some(bytes) = response.read()? {
            output.push(&bytes, &route.requested)?;
            if output.done {
                return Ok(());
            }
        }
        Err(Error::http(502, "AutoClaw 上游流提前中断，未收到完成标记")
            .with_code("runtime_stream_interrupted"))
    })();
    if let Err(error) = result {
        if output.started && !output.done {
            sdk::stream_event(&errors::event(&error))?;
        }
        return Err(error);
    }
    Ok(())
}
