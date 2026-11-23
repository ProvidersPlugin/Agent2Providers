//! 有界库库SSE事件读取：跨块保留UTF-8与data多行，HTTP200内ERROR也必须报失败。
//! 上游 MODEL_CALL_END / ACTUAL_POINT 计量原样保留，缺值不造0；EOF无终态不假成功。

use crate::{errors, util};
use agent2api_wasm_sdk::{http, sse::SseDecoder, Error, Result, Value};
use std::collections::VecDeque;

pub enum Event {
    Text(String),
    Reasoning(String),
    Usage(Value),
    Done,
}
pub struct Reader {
    response: http::Response,
    scanner: SseDecoder,
    pending: VecDeque<String>,
    ended: bool,
    done: bool,
    saw_text: bool,
    native_turn: bool,
}
impl Reader {
    pub fn new(response: http::Response) -> Self {
        Self {
            response,
            scanner: SseDecoder::default(),
            pending: VecDeque::new(),
            ended: false,
            done: false,
            saw_text: false,
            native_turn: false,
        }
    }
    fn finish(&mut self) -> Result<Option<Event>> {
        if !self.saw_text {
            return Err(Error::http(502, "库库回复已结束但没有返回任何正文"));
        }
        self.done = true;
        Ok(Some(Event::Done))
    }

    pub fn next(&mut self, secrets: &[&str]) -> Result<Option<Event>> {
        loop {
            if self.done {
                return Ok(None);
            }
            if let Some(raw) = self.pending.pop_front() {
                if raw.trim() == "[DONE]" {
                    return self.finish();
                }
                let value: Value = serde_json::from_str(&raw).map_err(|_| {
                    Error::http(502, "库库流式事件不是有效JSON").with_code("kuku_protocol")
                })?;
                if !value.is_object() {
                    return Err(Error::http(502, "库库流式事件不是对象"));
                }
                match value["type"].as_str() {
                    Some("TEXT_BLOCK_DELTA") => {
                        let delta = value["data"]["delta"]
                            .as_str()
                            .ok_or_else(|| Error::http(502, "库库文本增量格式无效"))?;
                        if !delta.is_empty() {
                            self.saw_text = true;
                            return Ok(Some(Event::Text(delta.into())));
                        }
                    }
                    Some("THINKING_BLOCK_DELTA") => {
                        let delta = value["data"]["delta"]
                            .as_str()
                            .ok_or_else(|| Error::http(502, "库库思考增量格式无效"))?;
                        if !delta.is_empty() {
                            return Ok(Some(Event::Reasoning(delta.into())));
                        }
                    }
                    Some("MODEL_CALL_END") => {
                        // 新客户端中 REPLY_END 仅是中间步骤，必须等本轮 TURN_DONE/FINISH。
                        self.native_turn = true;
                        let data = &value["data"];
                        let mut usage = serde_json::Map::new();
                        for (raw, public) in [
                            ("input_tokens", "prompt_tokens"),
                            ("output_tokens", "completion_tokens"),
                            ("reasoning_tokens", "reasoning_tokens"),
                            ("cache_read_tokens", "cache_read_tokens"),
                        ] {
                            if let Some(count) =
                                util::integer(&data[raw]).filter(|count| *count >= 0)
                            {
                                usage.insert(public.into(), Value::from(count));
                            }
                        }
                        if let Some(total) = usage
                            .get("prompt_tokens")
                            .and_then(Value::as_i64)
                            .zip(usage.get("completion_tokens").and_then(Value::as_i64))
                            .and_then(|(input, output)| input.checked_add(output))
                        {
                            usage.insert("total_tokens".into(), Value::from(total));
                        }
                        if !usage.is_empty() {
                            return Ok(Some(Event::Usage(Value::Object(usage))));
                        }
                    }
                    Some("ACTUAL_POINT") => {
                        self.native_turn = true;
                        if let Some(points) = util::number(value["data"].get("consume_points")) {
                            return Ok(Some(Event::Usage(serde_json::json!({"credit":points}))));
                        }
                    }
                    Some("REPLY_END" | "DIALOGUE_END") if !self.native_turn => {
                        return self.finish()
                    }
                    Some("TURN_DONE" | "FINISH") => return self.finish(),
                    Some("COMMERCIAL_NOTICE" | "COMMERCIAL") => {
                        let data = &value["data"];
                        let code =
                            util::integer(&data["errno"]).or_else(|| util::integer(&data["code"]));
                        let mut error = errors::upstream(502, code, data, secrets);
                        if code == Some(11002) || data["reason"] == "insufficient_balance" {
                            error.status = Some(429);
                            error.code = Some("kuku_quota".into());
                            error.message = "库库积分不足，无法继续本次任务".into();
                        }
                        return Err(error);
                    }
                    Some("REQUIRE_EXTERNAL_EXECUTION" | "AWAITING_INPUT") => {
                        return Err(Error::http(
                            422,
                            "库库正在等待官方客户端执行工具或补充输入，未自动重新发送任务",
                        )
                        .with_code("kuku_requires_client"));
                    }
                    Some("ERROR" | "STREAM_ERROR") => {
                        let code = util::integer(&value["data"]["code"])
                            .or_else(|| util::integer(&value["data"]["errno"]))
                            .or_else(|| util::integer(&value["data"]["error_code"]));
                        return Err(errors::upstream(502, code, &value["data"], secrets));
                    }
                    _ => {
                        if value.get("error").is_some_and(|value| !value.is_null()) {
                            return Err(errors::upstream(502, None, &value, secrets));
                        }
                    }
                }
                continue;
            }
            if self.ended {
                return Err(Error::http(502, "库库回复流提前结束，未收到完成标记")
                    .with_code("kuku_stream_incomplete"));
            }
            match self.response.read().map_err(errors::transport)? {
                Some(bytes) => {
                    if bytes.len() > 64 * 1024 {
                        return Err(Error::http(502, "库库宿主流读取块超出受控上限"));
                    }
                    self.pending.extend(
                        self.scanner
                            .push(&bytes)
                            .map_err(|_| Error::http(502, "库库流式事件过大或编码无效"))?,
                    );
                }
                None => {
                    self.ended = true;
                    self.pending.extend(
                        self.scanner
                            .push(b"\n\n")
                            .map_err(|_| Error::http(502, "库库尾部流式事件格式无效"))?,
                    );
                }
            }
        }
    }
}
