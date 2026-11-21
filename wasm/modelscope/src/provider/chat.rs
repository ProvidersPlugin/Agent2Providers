//! 标准 Chat SSE：原样保留工具/图片/思考字段以及组织/模型 ID，始终请求真实 usage。
//! 原生实测每个增量 usage 全零，只有 include_usage 打开后末尾空 choices 才带真值。

use agent2api_wasm_sdk::{self as sdk, http, json, sse::SseDecoder, Error, Result, Value};

use super::{account, client, errors, Region};

#[derive(Default)]
struct Output {
    pending: Vec<String>,
    pending_bytes: usize,
    started: bool,
    meaningful: bool,
    complete: bool,
}

impl Output {
    fn start(&mut self) -> Result<()> {
        if !self.started {
            sdk::stream_head(200)?;
            self.started = true;
        }
        for data in self.pending.drain(..) {
            sdk::stream_bytes(format!("data: {data}\n\n").as_bytes())?;
        }
        self.pending_bytes = 0;
        Ok(())
    }

    fn event(&mut self, region: Region, data: &str) -> Result<()> {
        if self.complete {
            return Ok(());
        }
        if data.trim() == "[DONE]" {
            if !self.meaningful {
                return Err(Error::http(502, "ModelScope 未返回有效内容或结束原因"));
            }
            self.start()?;
            sdk::stream_bytes(b"data: [DONE]\n\n")?;
            self.complete = true;
            return Ok(());
        }
        let value: Value = serde_json::from_str(data)
            .map_err(|_| Error::http(502, "ModelScope 流式事件不是有效 JSON"))?;
        if !client::succeeded(&value) {
            return Err(stream_error(region, &value));
        }
        let choices = value["choices"]
            .as_array()
            .ok_or_else(|| Error::http(502, "ModelScope 流式事件缺少 choices"))?;
        for choice in choices {
            if !choice.is_object() {
                return Err(Error::http(502, "ModelScope 流式 choice 格式无效"));
            }
            if choice.get("error").is_some_and(|error| !error.is_null()) {
                return Err(stream_error(region, choice));
            }
            if matches!(
                choice["finish_reason"].as_str(),
                Some("content_filter" | "safety")
            ) {
                return Err(errors::http_error(
                    region,
                    403,
                    &json!({ "message": "content_filter" }),
                ));
            }
            let delta = &choice["delta"];
            self.meaningful |= ["content", "reasoning_content", "reasoning"]
                .iter()
                .any(|key| delta[*key].as_str().is_some_and(|text| !text.is_empty()))
                || delta["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| !calls.is_empty())
                || delta["function_call"].is_object()
                || choice["finish_reason"].as_str().is_some();
        }
        // 不把中间全零 usage 丢掉，也不能在看到 finish_reason 时提前停止：
        // usage-only 真值帧位于它后面，必须一直读到 [DONE]。
        if self.started {
            sdk::stream_bytes(format!("data: {data}\n\n").as_bytes())?;
        } else {
            self.pending_bytes = self.pending_bytes.saturating_add(data.len());
            if (self.pending_bytes > 64 * 1024 || self.pending.len() >= 128) && !self.meaningful {
                return Err(Error::http(
                    502,
                    "ModelScope 流中未出现有效内容，前置事件超过限制",
                ));
            }
            self.pending.push(data.to_string());
            if self.meaningful {
                self.start()?;
            }
        }
        Ok(())
    }

    fn consume(
        &mut self,
        region: Region,
        source: &account::Snapshot,
        mut response: http::Response,
    ) -> Result<()> {
        let content_type = response.headers["content-type"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase();
        if content_type.contains("application/json") {
            let bytes = response.bytes(4 * 1024 * 1024)?;
            source.check()?;
            let mut value: Value = serde_json::from_slice(&bytes)
                .map_err(|_| Error::http(502, "ModelScope 未返回有效的模型响应"))?;
            if !client::succeeded(&value) {
                return Err(stream_error(region, &value));
            }
            let choices = value["choices"]
                .as_array()
                .ok_or_else(|| Error::http(502, "ModelScope 模型响应缺少 choices"))?;
            let mut deltas = Vec::with_capacity(choices.len());
            for (index, choice) in choices.iter().enumerate() {
                if !choice.is_object() || !choice["message"].is_object() {
                    return Err(Error::http(502, "ModelScope 模型响应缺少 message"));
                }
                let mut choice = choice.clone();
                let mut delta = choice["message"].clone();
                if let Some(calls) = delta["tool_calls"].as_array_mut() {
                    for (index, call) in calls.iter_mut().enumerate() {
                        if call.is_object() {
                            call["index"] = json!(index);
                        }
                    }
                }
                choice["delta"] = delta;
                if choice["index"].is_null() {
                    choice["index"] = json!(index);
                }
                if let Some(choice) = choice.as_object_mut() {
                    choice.remove("message");
                }
                deltas.push(choice);
            }
            value["object"] = json!("chat.completion.chunk");
            value["choices"] = json!(deltas);
            self.event(region, &value.to_string())?;
            return self.event(region, "[DONE]");
        }
        if content_type.contains("text/html") {
            return Err(Error::http(502, "ModelScope 返回了网页而不是模型回答"));
        }
        let mut decoder = SseDecoder::default();
        while let Some(bytes) = response.read()? {
            source.check()?;
            for data in decoder.push(&bytes)? {
                self.event(region, &data)?;
            }
            if self.complete {
                return source.check();
            }
        }
        source.check()?;
        // 允许最后的 [DONE] 未附空行；仍不能把缺少结束标志的断流算成功。
        for data in decoder.push(b"\n\n")? {
            self.event(region, &data)?;
        }
        if self.complete {
            Ok(())
        } else {
            Err(Error::http(
                502,
                "ModelScope 上游流提前中断，未收到完整结束标记",
            ))
        }
    }
}

fn stream_error(region: Region, value: &Value) -> Error {
    let status = ["status", "status_code", "code"]
        .iter()
        .find_map(|key| {
            [&value["error"][*key], &value[*key]]
                .into_iter()
                .find_map(|field| {
                    field
                        .as_u64()
                        .or_else(|| field.as_str().and_then(|text| text.parse().ok()))
                        .filter(|status| (400..600).contains(status))
                })
        })
        .unwrap_or(502) as u16;
    errors::http_error(region, status, value)
}

pub fn forward(region: Region, mut params: Value) -> Result<()> {
    let source = account::Snapshot::read(region)?;
    let token = &source.token;
    let mut body = sdk::take_body(&mut params).unwrap_or(Value::Null);
    drop(params);
    let object = body
        .as_object_mut()
        .ok_or_else(|| Error::http(400, "请求体必须是 JSON 对象"))?;
    object.insert("stream".to_string(), json!(true));
    let mut options = object
        .get("stream_options")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    options.insert("include_usage".to_string(), json!(true));
    object.insert("stream_options".to_string(), Value::Object(options));
    source.check()?;
    let response = http::open(
        "POST",
        &format!("{}/chat/completions", region.inference_base()),
        json!({ "Authorization": format!("Bearer {token}"),
            "Accept": "text/event-stream", "Content-Type": "application/json" }),
        Some(&body),
    )?;
    source.check()?;
    if !(200..300).contains(&response.status) {
        let status = response.status;
        let bytes = response.bytes(1024 * 1024)?;
        source.check()?;
        let mut error = errors::from_bytes(region, status, &bytes);
        error.message = error.message.replace(token, "[凭据已隐藏]");
        return Err(error);
    }
    let mut output = Output::default();
    match output.consume(region, &source, response) {
        Ok(()) => source.check(),
        Err(mut error) => {
            error.message = error.message.replace(token, "[凭据已隐藏]");
            if output.started {
                sdk::stream_event(&errors::event(&error))?;
            }
            Err(error)
        }
    }
}
