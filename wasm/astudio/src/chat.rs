//! AStudio Responses SSE的业务错误门与少量事件别名适配；真正协议转换只用宿主Converter。
//! response.created的空role不提前发响应头；开始输出后不重放请求，缺明确终态不补成功。

use super::{account, client, errors, models, payload};
use agent2api_wasm_sdk::{
    self as sdk, http, json, protocol::Converter, sse::SseDecoder, Error, Result, Value,
};
use std::collections::HashSet;

#[derive(Default)]
struct Output {
    decoder: SseDecoder,
    pending: Vec<u8>,
    started: bool,
    finished: bool,
    done: bool,
}

impl Output {
    fn emit(&mut self, bytes: &[u8], useful: bool) -> Result<()> {
        if self.started {
            return sdk::stream_bytes(bytes);
        }
        if self.pending.len().saturating_add(bytes.len()) > 1024 * 1024 {
            return Err(Error::http(502, "AStudio 连续返回空事件，没有有效回答"));
        }
        self.pending.extend_from_slice(bytes);
        if useful {
            sdk::stream_head(200)?;
            self.started = true;
            sdk::stream_bytes(&self.pending)?;
            self.pending.clear();
        }
        Ok(())
    }

    fn push(&mut self, bytes: &[u8]) -> Result<()> {
        for data in self.decoder.push(bytes)? {
            if self.done {
                continue;
            }
            if data.trim() == "[DONE]" {
                if !self.finished {
                    return Err(Error::http(502, "AStudio 转换流缺少有效结束原因"));
                }
                self.emit(b"data: [DONE]\n\n", true)?;
                self.done = true;
                continue;
            }
            let value: Value = serde_json::from_str(&data)
                .map_err(|_| Error::http(502, "AStudio 协议转换返回了无效事件"))?;
            if let Some(error) = errors::event(&value) {
                return Err(error);
            }
            let choices = value["choices"]
                .as_array()
                .ok_or_else(|| Error::http(502, "AStudio 转换事件缺少choices"))?;
            let mut useful = false;
            for choice in choices {
                let delta = &choice["delta"];
                useful |= ["content", "reasoning_content", "reasoning"]
                    .iter()
                    .any(|key| delta[*key].as_str().is_some_and(|text| !text.is_empty()))
                    || delta["tool_calls"]
                        .as_array()
                        .is_some_and(|items| !items.is_empty())
                    || delta["_wb_encrypted_content"]
                        .as_str()
                        .is_some_and(|value| !value.is_empty());
                if let Some(reason) = choice["finish_reason"]
                    .as_str()
                    .filter(|reason| !reason.is_empty())
                {
                    if matches!(reason, "content_filter" | "safety") {
                        return Err(errors::upstream(
                            403,
                            &json!({ "providerCode": "astudio_content" }),
                            "model",
                        ));
                    }
                    self.finished = true;
                    useful = true;
                }
            }
            self.emit(format!("data: {value}\n\n").as_bytes(), useful)?;
        }
        Ok(())
    }
}

struct Events {
    text: HashSet<String>,
    terminal: bool,
}

impl Events {
    fn new() -> Self {
        Self {
            text: HashSet::new(),
            terminal: false,
        }
    }

    fn slots(event: &Value) -> Vec<String> {
        // item_id 与 output_index 是片段别名，某帧少带一个别名不应重复输出 done 快照。
        let content = event["content_index"].as_u64().unwrap_or(0);
        let mut slots = Vec::new();
        if let Some(index) = event["output_index"].as_u64() {
            slots.push(format!("output:{index}:{content}"));
        }
        if let Some(id) = event["item_id"].as_str().filter(|value| !value.is_empty()) {
            slots.push(format!("item:{id}:{content}"));
        }
        if slots.is_empty() {
            slots.push(format!("anonymous:{content}"));
        }
        slots
    }

    fn normalize(&mut self, mut event: Value) -> Result<Option<Value>> {
        if let Some(error) = errors::event(&event) {
            return Err(error);
        }
        if event["data"]["type"].is_string() && errors::code(&event) == Some(0) {
            event = event["data"].clone();
            if let Some(error) = errors::event(&event) {
                return Err(error);
            }
        }
        let mut kind = event["type"]
            .as_str()
            .filter(|kind| !kind.is_empty())
            .ok_or_else(|| Error::http(502, "AStudio Responses事件缺少类型"))?
            .to_string();
        kind = match kind.as_str() {
            "response.text.delta" => "response.output_text.delta".into(),
            "response.text.done" => "response.output_text.done".into(),
            "response.reasoning.delta" => "response.reasoning_text.delta".into(),
            "response.done" => "response.completed".into(),
            _ => kind,
        };
        if kind == "response.completed" && event["response"]["status"] == "incomplete" {
            kind = "response.incomplete".into();
        }
        if kind == "response.incomplete" {
            let reason = event
                .pointer("/response/incomplete_details/reason")
                .or_else(|| event.pointer("/incomplete_details/reason"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if reason == "content_filter" {
                return Err(errors::upstream(
                    403,
                    &json!({ "providerCode": "astudio_content" }),
                    "model",
                ));
            }
            if !matches!(reason, "" | "max_output_tokens") {
                return Err(Error::http(502, "AStudio 未完成回答，截断原因未能确认"));
            }
            if reason.is_empty() {
                return Err(Error::http(502, "AStudio 回答未完成且缺少截断原因"));
            }
        }
        if matches!(kind.as_str(), "response.completed" | "response.incomplete") {
            if event
                .get("response")
                .is_some_and(|value| !value.is_object())
            {
                return Err(Error::http(502, "AStudio Responses结束事件载荷无效"));
            }
            if event["response"].get("status").is_some_and(|value| {
                value
                    .as_str()
                    .is_none_or(|status| !matches!(status, "completed" | "incomplete"))
            }) {
                return Err(Error::http(502, "AStudio 结束事件未确认回答完成或截断"));
            }
            self.terminal = true;
        }
        // 原生支持done-only正文，通用Converter只消费delta。仅对本家补别名，不复制协议转换树。
        let slots = Self::slots(&event);
        if matches!(
            kind.as_str(),
            "response.output_text.delta" | "response.output_text.done"
        ) {
            if kind.ends_with(".delta") {
                if event["delta"].as_str().is_some_and(|text| !text.is_empty()) {
                    self.text.extend(slots);
                }
            } else {
                if slots.iter().any(|slot| self.text.contains(slot)) {
                    return Ok(None);
                }
                let text = event["text"].as_str().unwrap_or("");
                if text.is_empty() {
                    return Ok(None);
                }
                event["delta"] = json!(text);
                kind = "response.output_text.delta".into();
                self.text.extend(slots);
            }
        }
        if self.text.len() > 4096 {
            return Err(Error::http(502, "AStudio 回答片段数量超过限制"));
        }
        event["type"] = json!(kind);
        Ok(Some(event))
    }
}

fn consume(
    mut response: http::Response,
    model: &str,
    output: &mut Output,
    record: &Value,
) -> Result<()> {
    let content_type = response.headers["content-type"]
        .as_str()
        .unwrap_or("")
        .to_ascii_lowercase();
    if content_type.contains("application/json") {
        let reply = client::read(response)?;
        if let Some(error) = errors::event(&reply.body) {
            return Err(error);
        }
        if let Some(error) = errors::event(&reply.body["data"]) {
            return Err(error);
        }
        return Err(Error::http(
            502,
            "AStudio 返回了JSON而不是请求的Responses流",
        ));
    }
    if content_type.contains("text/html") {
        return Err(Error::http(502, "AStudio 返回了网页而不是模型回答"));
    }
    let mut decoder = SseDecoder::default();
    let mut events = Events::new();
    let mut converter = Converter::new("responses", model)?;
    let mut eof = false;
    while !eof {
        let bytes = match response.read().map_err(errors::transport)? {
            Some(bytes) => bytes,
            None => {
                eof = true;
                b"\n\n".to_vec()
            }
        };
        // EOF 只冲刷已经收到的最后事件；仍必须有真实 Responses 完成/截断终态。
        if !output.started {
            account::ensure_current(record)?;
        }
        for data in decoder.push(&bytes)? {
            if data.trim() == "[DONE]" {
                return Err(Error::http(
                    502,
                    "AStudio 在完整Responses结束事件前返回DONE",
                ));
            }
            let event: Value = serde_json::from_str(&data)
                .map_err(|_| Error::http(502, "AStudio 流式事件不是有效JSON"))?;
            if let Some(event) = events.normalize(event)? {
                output.push(&converter.push(format!("data: {event}\n\n").as_bytes())?)?;
            }
            if events.terminal {
                output.push(&converter.finish()?)?;
                return if output.done {
                    Ok(())
                } else {
                    Err(Error::http(502, "AStudio 转换流未完整结束"))
                };
            }
        }
    }
    Err(Error::http(502, "AStudio 流在完整Responses结束事件前中断"))
}

pub fn forward(params: &Value) -> Result<()> {
    let record = account::current()?;
    let model = payload::model(params)?;
    models::authorize(&record["secret"], &model)?;
    let body = payload::build(&params["body"], &model)?;
    account::ensure_current(&record)?;
    let response = client::responses(&account::model_key(&record["secret"])?, &body)?;
    if !(200..300).contains(&response.status) {
        let reply = client::read(response)?;
        return Err(errors::upstream(reply.status, &reply.body, "model"));
    }
    let mut output = Output::default();
    match consume(response, &model, &mut output, &record) {
        Ok(()) => Ok(()),
        Err(error) if output.started => {
            sdk::stream_event(&errors::stream(&error))?;
            Err(error)
        }
        Err(error) => Err(error),
    }
}
