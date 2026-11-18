//! 先识别上游业务错误与结束事件，再送入宿主通用协议转换；流中断不补成功。
use agent2api_wasm_sdk::{
    self as sdk, http, json, protocol::Converter, sse::SseDecoder, Error, Result, Value,
};

use crate::{account, errors, util};

#[derive(Default)]
pub struct Output {
    decoder: SseDecoder,
    pending: Vec<u8>,
    pub started: bool,
    finish: bool,
    done: bool,
}

impl Output {
    fn event(&mut self, raw: &str, protected: &[String]) -> Result<()> {
        if self.done {
            return Ok(());
        }
        if raw.trim() == "[DONE]" {
            if !self.finish {
                return Err(Error::http(502, "科搭没有返回有效结束原因"));
            }
            self.emit(b"data: [DONE]\n\n", true)?;
            self.done = true;
            return Ok(());
        }
        let value: Value =
            serde_json::from_str(raw).map_err(|_| Error::http(502, "科搭对话事件不是有效 JSON"))?;
        if errors::failed(&value) {
            return Err(errors::upstream(200, &value, protected));
        }
        let choices = value["choices"]
            .as_array()
            .ok_or_else(|| Error::http(502, "科搭标准流事件缺少 choices"))?;
        let mut useful = false;
        for choice in choices {
            if choice["finish_reason"] == "content_filter" {
                return Err(errors::upstream(
                    403,
                    &json!({ "error": { "type": "content_filter" } }),
                    protected,
                ));
            }
            useful |= ["content", "reasoning_content", "reasoning"]
                .iter()
                .any(|key| {
                    choice["delta"][*key]
                        .as_str()
                        .is_some_and(|text| !text.is_empty())
                })
                || choice["delta"]["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| !calls.is_empty());
            if choice["finish_reason"]
                .as_str()
                .is_some_and(|reason| !reason.is_empty())
            {
                self.finish = true;
                useful = true;
            }
        }
        self.emit(format!("data: {raw}\n\n").as_bytes(), useful)
    }
    fn emit(&mut self, bytes: &[u8], useful: bool) -> Result<()> {
        if self.started {
            return sdk::stream_bytes(bytes);
        }
        if self.pending.len().saturating_add(bytes.len()) > 256 * 1024 {
            return Err(Error::http(502, "科搭持续返回空事件，没有有效回答"));
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
    fn push(&mut self, bytes: &[u8], protected: &[String]) -> Result<()> {
        for raw in self.decoder.push(bytes)? {
            self.event(&raw, protected)?;
        }
        Ok(())
    }
    pub fn fail(&self, error: &Error) -> Result<()> {
        if self.started {
            sdk::stream_event(&errors::event(error))?;
        }
        Ok(())
    }
}

fn terminal(protocol: &str, value: &Value, raw: &str) -> Result<bool> {
    match protocol {
        "chat" => Ok(raw.trim() == "[DONE]"),
        "anthropic" => Ok(value["type"] == "message_stop"),
        "responses" => {
            if value["type"] == "response.incomplete" {
                if value["response"]["incomplete_details"]["reason"] != "max_output_tokens" {
                    return Err(Error::http(
                        502,
                        "科搭 Responses 回答未完成且不是正常长度截断",
                    ));
                }
                return Ok(true);
            }
            Ok(value["type"] == "response.completed")
        }
        _ => Err(Error::http(400, "科搭对话协议未接入")),
    }
}

pub fn consume(
    mut response: http::Response,
    protocol: &str,
    model: &str,
    output: &mut Output,
    protected: &[String],
    current: &account::Snapshot,
) -> Result<()> {
    let content_type = response.headers["content-type"]
        .as_str()
        .unwrap_or("")
        .to_ascii_lowercase();
    if content_type.contains("application/json") {
        let reply = crate::network::read(response, protected)?;
        if errors::failed(&reply.body) {
            return Err(errors::upstream(reply.status, &reply.body, protected));
        }
        return Err(Error::http(502, "科搭没有返回请求要求的流式回答"));
    }
    if content_type.contains("text/html") {
        return Err(Error::http(502, "科搭返回了网页而不是模型回答"));
    }
    let mut decoder = SseDecoder::default();
    let mut converter = if protocol == "chat" {
        None
    } else {
        Some(Converter::new(protocol, model)?)
    };
    let mut total = 0usize;
    while let Some(bytes) = response
        .read()
        .map_err(|error| util::redact(error, protected))?
    {
        current.unchanged()?;
        total = total.saturating_add(bytes.len());
        if total > 64 * 1024 * 1024 {
            return Err(Error::http(502, "科搭单次回答超过64 MiB限制"));
        }
        for raw in decoder.push(&bytes)? {
            let value: Value = if raw.trim() == "[DONE]" {
                Value::Null
            } else {
                serde_json::from_str(&raw).map_err(|_| Error::http(502, "科搭流式事件格式无效"))?
            };
            if errors::failed(&value) {
                return Err(errors::upstream(200, &value, protected));
            }
            let ended = terminal(protocol, &value, &raw)?;
            let event = format!("data: {raw}\n\n");
            let converted = match &mut converter {
                Some(converter) => converter.push(event.as_bytes())?,
                None => event.into_bytes(),
            };
            output.push(&converted, protected)?;
            if ended {
                current.unchanged()?;
                if current.secret["source"] == "desktop"
                    && current.access()?.as_str()
                        != protected.first().map(String::as_str).unwrap_or("")
                {
                    return Err(errors::changed());
                }
                if let Some(converter) = &mut converter {
                    output.push(&converter.finish()?, protected)?;
                }
                if !output.done && output.finish {
                    output.event("[DONE]", protected)?;
                }
                return if output.done {
                    Ok(())
                } else {
                    Err(Error::http(502, "科搭协议转换未完整结束"))
                };
            }
        }
    }
    Err(Error::http(502, "科搭上游流提前中断，没有完整结束标记")
        .with_code("codar_stream_incomplete"))
}
