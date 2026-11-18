//! 按本账号目录选择 Chat / Messages；输出完整帧之后才发送，不把截断 DONE 视为成功。
use super::{account, errors, headers, models, MODEL_BASE};
use agent2api_wasm_sdk::{
    self as sdk, http, json, protocol, sse::SseDecoder, Error, Result, Value,
};
use std::collections::BTreeMap;

#[derive(Default)]
struct Evidence {
    choices: BTreeMap<u64, bool>,
    anthropic_started: bool,
    anthropic_stop: bool,
    output: bool,
}

impl Evidence {
    fn inspect(&mut self, event: &str, kind: &str, session: &str) -> Result<bool> {
        if event.trim() == "[DONE]" {
            if kind != "chat"
                || self.choices.is_empty()
                || !self.choices.values().all(|finished| *finished)
            {
                return Err(Error::http(
                    502,
                    "Loomy 上游结束但未返回完整完成原因，未视为成功",
                ));
            }
            return Ok(true);
        }
        let value: Value = serde_json::from_str(event)
            .map_err(|_| Error::http(502, "Loomy 上游流式事件格式无效"))?;
        let code = errors::business_code(&value);
        if value.get("error").is_some_and(|error| !error.is_null())
            || value["type"] == "error"
            || !code.is_empty() && code != errors::SUCCESS
        {
            return Err(stream_error(&value, session));
        }
        if kind == "chat" {
            let choices = value["choices"]
                .as_array()
                .ok_or_else(|| Error::http(502, "Loomy 流式响应缺少 choices"))?;
            for (position, choice) in choices.iter().enumerate() {
                let index = choice["index"].as_u64().unwrap_or(position as u64);
                let finished = choice["finish_reason"]
                    .as_str()
                    .is_some_and(|reason| !reason.trim().is_empty());
                self.choices
                    .entry(index)
                    .and_modify(|old| *old |= finished)
                    .or_insert(finished);
                self.output |= choice["delta"]["content"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty())
                    || choice["delta"]["reasoning_content"]
                        .as_str()
                        .is_some_and(|text| !text.is_empty())
                    || choice["delta"]["tool_calls"]
                        .as_array()
                        .is_some_and(|items| !items.is_empty());
            }
        } else {
            self.anthropic_started |= value["type"] == "message_start";
            self.anthropic_stop |= value["type"] == "message_delta"
                && value["delta"]["stop_reason"]
                    .as_str()
                    .is_some_and(|reason| !reason.trim().is_empty());
            if value["type"] == "message_stop" {
                if !self.anthropic_started || !self.anthropic_stop {
                    return Err(Error::http(502, "Loomy Messages 缺少完整开始或完成原因"));
                }
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[derive(Default)]
struct Output {
    started: bool,
    complete: bool,
    decoder: SseDecoder,
}

impl Output {
    fn emit(&mut self, bytes: &[u8], session: &str, record: &Value) -> Result<()> {
        for event in self.decoder.push(bytes)? {
            if self.complete {
                return Err(Error::http(502, "Loomy 转换器在结束后仍返回内容"));
            }
            if event.trim() == "[DONE]" {
                self.complete = true;
            } else {
                let value: Value = serde_json::from_str(&event)
                    .map_err(|_| Error::http(502, "Loomy 返回无效流式 JSON"))?;
                if value.get("error").is_some_and(|error| !error.is_null()) {
                    return Err(stream_error(&value, session));
                }
                if !value["choices"].is_array() {
                    return Err(Error::http(502, "Loomy 流式响应缺少 choices"));
                }
            }
            account::ensure_current(record)?;
            if !self.started {
                sdk::stream_head(200)?;
                self.started = true;
            }
            sdk::stream_bytes(format!("data: {event}\n\n").as_bytes())?;
        }
        Ok(())
    }

    fn consume(
        &mut self,
        mut response: http::Response,
        kind: &str,
        model: &str,
        session: &str,
        record: &Value,
    ) -> Result<()> {
        let mut converter = protocol::Converter::new(kind, model)?;
        let mut upstream = SseDecoder::default();
        let mut evidence = Evidence::default();
        let mut terminal = false;
        while let Some(bytes) = response.read()? {
            account::ensure_current(record)?;
            for event in upstream.push(&bytes)? {
                if terminal {
                    continue;
                }
                terminal = evidence.inspect(&event, kind, session)?;
                let converted = converter.push(format!("data: {event}\n\n").as_bytes())?;
                self.emit(&converted, session, record)?;
            }
            if terminal {
                break;
            }
        }
        if !terminal {
            for event in upstream.push(b"\n\n")? {
                if terminal {
                    continue;
                }
                terminal = evidence.inspect(&event, kind, session)?;
                let converted = converter.push(format!("data: {event}\n\n").as_bytes())?;
                self.emit(&converted, session, record)?;
            }
        }
        if !terminal {
            return Err(Error::http(502, "Loomy 上游流提前中断，未收到完整结束事件"));
        }
        let tail = converter.finish()?;
        self.emit(&tail, session, record)?;
        self.emit(b"\n\n", session, record)?;
        if !self.complete {
            return Err(Error::http(502, "Loomy 协议转换缺少完整结束标记"));
        }
        account::ensure_current(record)
    }
}

fn stream_error(value: &Value, session: &str) -> Error {
    let status = value["error"]["status"]
        .as_u64()
        .or_else(|| value["status"].as_u64())
        .filter(|status| (400..600).contains(status))
        .unwrap_or(502) as u16;
    errors::http_error(status, value, &[session])
}

pub fn forward(params: &Value) -> Result<()> {
    let record = account::current()?;
    let session = account::token(&record["secret"])?;
    let mut body = params["body"].clone();
    if !body.is_object() {
        return Err(Error::http(400, "Loomy 请求体必须是 JSON 对象"));
    }
    let model = body["model"]
        .as_str()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| Error::http(400, "Loomy 请求缺少模型名"))?
        .to_string();
    body["model"] = json!(model);
    body["stream"] = json!(true);
    let anthropic = models::protocol(&model, &session)? == "anthropic";
    super::history::adapt(&mut body, &model)?;
    let mut headers = headers::build(&session, &params["headers"], false)?;
    let path = if anthropic {
        body = protocol::request("anthropic", &body, &model)?;
        headers["anthropic-version"] = json!("2023-06-01");
        headers["x-api-key"] = json!(session);
        "messages"
    } else {
        "chat/completions"
    };
    account::ensure_current(&record)?;
    let response = http::open(
        "POST",
        &format!("{MODEL_BASE}/{path}"),
        headers,
        Some(&body),
    )?;
    if errors::is_error_response(response.status, &response.headers) {
        let status = response.status;
        let bytes = response.bytes(1024 * 1024)?;
        let payload: Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({ "message": String::from_utf8_lossy(&bytes) }));
        account::ensure_current(&record)?;
        return Err(errors::http_error(status, &payload, &[&session]));
    }
    let mut output = Output::default();
    let result = output.consume(
        response,
        if anthropic { "anthropic" } else { "chat" },
        &model,
        &session,
        &record,
    );
    match result {
        Err(error) if output.started => {
            sdk::stream_event(&errors::event(&error))?;
            Err(error)
        }
        result => result,
    }
}
