use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{json, Value};

use super::api;
use crate::errors::GatewayError;

const MAX_OUTPUT: usize = 16 * 1024 * 1024;
const MAX_EVENT: usize = 2 * 1024 * 1024;

#[derive(Default)]
struct SseParser {
    pending: Vec<u8>,
}

impl SseParser {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<(String, String)>, GatewayError> {
        self.pending.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            let separator = [
                self.pending
                    .windows(2)
                    .position(|value| value == b"\n\n")
                    .map(|at| (at, 2)),
                self.pending
                    .windows(4)
                    .position(|value| value == b"\r\n\r\n")
                    .map(|at| (at, 4)),
            ]
            .into_iter()
            .flatten()
            .min_by_key(|(at, _)| *at);
            let Some((at, length)) = separator else {
                break;
            };
            if at > MAX_EVENT {
                return Err(GatewayError::with_status(502, "豆包工作流式事件过大"));
            }
            let raw = self.pending.drain(..at + length).collect::<Vec<_>>();
            let block = std::str::from_utf8(&raw[..at])
                .map_err(|_| GatewayError::with_status(502, "豆包工作流式事件不是有效 UTF-8"))?;
            let mut event = String::new();
            let mut data = Vec::new();
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("event:") {
                    event = value.trim_start().to_string();
                } else if let Some(value) = line.strip_prefix("data:") {
                    data.push(value.strip_prefix(' ').unwrap_or(value));
                }
            }
            if !data.is_empty() {
                frames.push((event, data.join("\n")));
            }
        }
        if self.pending.len() > MAX_EVENT {
            return Err(GatewayError::with_status(502, "豆包工作流式事件过大"));
        }
        Ok(frames)
    }
}

#[derive(Default)]
pub struct Decoder {
    pub text: String,
    pub reasoning: String,
    reasoning_blocks: BTreeSet<String>,
    blocks: BTreeMap<String, String>,
    pub done: bool,
    received: usize,
}

#[derive(Clone)]
pub enum Delta {
    Text(String),
    Reasoning(String),
}

impl Decoder {
    fn blocks(&mut self, value: &Value, append: bool) -> Result<Vec<Delta>, GatewayError> {
        let mut output = Vec::new();
        for block in value.as_array().into_iter().flatten() {
            let kind = api::number(&block["block_type"]);
            if kind == Some(10040) {
                continue;
            }
            if kind != Some(10000) {
                continue;
            }
            let Some(text) = block["content"]["text_block"]["text"].as_str() else {
                continue;
            };
            let id = api::identifier(&block["block_id"]);
            let icons = ["icon_url", "icon_url_dark"];
            let tagged = icons.iter().any(|key| {
                block["content"]["text_block"][*key]
                    .as_str()
                    .is_some_and(|icon| icon.to_ascii_lowercase().contains("deep_think"))
            });
            if let Some(id) = id.as_ref() {
                if tagged {
                    self.reasoning_blocks.insert(id.clone());
                } else if icons
                    .iter()
                    .any(|key| block["content"]["text_block"].get(*key).is_some())
                {
                    self.reasoning_blocks.remove(id);
                }
            }
            let reasoning = tagged
                || id
                    .as_ref()
                    .is_some_and(|id| self.reasoning_blocks.contains(id));
            let key = format!(
                "{}:{}",
                if reasoning { "reasoning" } else { "text" },
                id.unwrap_or_else(|| "default".into())
            );
            let prior = self.blocks.entry(key).or_default();
            let delta = if append {
                prior.push_str(text);
                text.to_string()
            } else if let Some(suffix) = text.strip_prefix(prior.as_str()) {
                let suffix = suffix.to_string();
                *prior = text.into();
                suffix
            } else if prior.is_empty() {
                *prior = text.into();
                text.to_string()
            } else {
                return Err(GatewayError::with_status(
                    502,
                    "豆包工作替换了已发送的回答，无法安全继续流式响应",
                ));
            };
            if delta.is_empty() {
                continue;
            }
            if reasoning {
                self.reasoning.push_str(&delta);
                output.push(Delta::Reasoning(delta));
            } else {
                self.text.push_str(&delta);
                output.push(Delta::Text(delta));
            }
            if self.text.len() > MAX_OUTPUT || self.reasoning.len() > MAX_OUTPUT {
                return Err(GatewayError::with_status(502, "豆包工作输出超过大小限制"));
            }
        }
        Ok(output)
    }

    fn frame(&mut self, event: &str, raw: &str) -> Result<Vec<Delta>, GatewayError> {
        self.received = self.received.saturating_add(raw.len());
        if self.received > MAX_OUTPUT * 4 {
            return Err(GatewayError::with_status(502, "豆包工作流式响应过大"));
        }
        let value: Value = serde_json::from_str(raw)
            .map_err(|_| GatewayError::with_status(502, "豆包工作流式事件格式无效"))?;
        if event.contains("ERR")
            || event == "gateway-error"
            || event == "error"
            || value
                .get("error")
                .is_some_and(|error| !error.is_null() && error != &Value::Bool(false))
            || api::number(&value["code"]).is_some_and(|code| code != 0)
            || api::number(&value["error_code"]).is_some_and(|code| code != 0)
        {
            return Err(api::error(200, &value));
        }
        if self.done {
            return Ok(Vec::new());
        }
        match event {
            "SSE_REPLY_END" => { self.done = true; Ok(Vec::new()) },
            "STREAM_MSG_NOTIFY" => self.blocks(&value["content"]["content_block"], true),
            "CHUNK_DELTA" if value["text"].is_string() => self.blocks(&json!([{"block_id":"delta","block_type":10000,"content":{"text_block":{"text":value["text"]}}}]), true),
            "FULL_MSG_NOTIFY" if api::number(&value["message"]["user_type"]) == Some(2) => self.blocks(&value["message"]["content_block"], false),
            "STREAM_CHUNK" => {
                let mut deltas = Vec::new();
                for patch in value["patch_op"].as_array().into_iter().flatten() {
                    if api::number(&patch["patch_object"]) != Some(1) { continue; }
                    deltas.extend(self.blocks(&patch["patch_value"]["content_block"], api::number(&patch["patch_type"]) == Some(1))?);
                }
                Ok(deltas)
            }
            _ => Ok(Vec::new()),
        }
    }
}

pub struct Turn {
    source: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    parser: SseParser,
    pending: VecDeque<(String, String)>,
    pub decoder: Decoder,
    received: usize,
    deltas: VecDeque<Delta>,
    idle: Duration,
}

impl Turn {
    pub fn new(response: reqwest::Response) -> Self {
        Self {
            source: Box::pin(response.bytes_stream()),
            parser: SseParser::default(),
            pending: VecDeque::new(),
            decoder: Decoder::default(),
            received: 0,
            deltas: VecDeque::new(),
            idle: Duration::from_millis(crate::timeouts::current().stream_idle_ms()),
        }
    }

    pub async fn next(&mut self) -> Result<Option<Delta>, GatewayError> {
        loop {
            if let Some(delta) = self.deltas.pop_front() {
                return Ok(Some(delta));
            }
            if let Some((event, data)) = self.pending.pop_front() {
                self.deltas.extend(self.decoder.frame(&event, &data)?);
                continue;
            }
            if self.decoder.done {
                return Ok(None);
            }
            let chunk = tokio::time::timeout(self.idle, self.source.next())
                .await
                .map_err(|_| GatewayError::with_status(504, "豆包工作流式响应空闲超时"))?;
            match chunk {
                Some(Ok(bytes)) => {
                    self.received = self.received.saturating_add(bytes.len());
                    if self.received > MAX_OUTPUT * 4 {
                        return Err(GatewayError::with_status(502, "豆包工作流式响应过大"));
                    }
                    self.pending.extend(self.parser.push(&bytes)?);
                }
                Some(Err(error)) => {
                    return Err(GatewayError::with_status(
                        502,
                        crate::http::describe(&error),
                    ))
                }
                None => {
                    return Err(GatewayError::with_status(
                        502,
                        "豆包工作响应在 SSE_REPLY_END 之前中断",
                    ))
                }
            }
        }
    }
}
