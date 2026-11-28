//! 双通道共用标准 Chat SSE 输出。纯协议转换由宿主 protocol.convert.v1 承担，
//! 本家保留HTTP 200业务拒绝、风控停止、验证码与Start Plan三块提示等专属规则。
use super::{account, captcha, config, errors, network, prompt, quota, reasoning, request_context};
use agent2api_wasm_sdk::{
    self as sdk, http, json, protocol, sse::SseDecoder, Error, Result, Value,
};

struct Output {
    decoder: SseDecoder,
    started: bool,
    done: bool,
    content: bool,
    finish: bool,
    pending: Vec<Value>,
    pending_bytes: usize,
}
impl Output {
    fn new() -> Self {
        Self {
            decoder: SseDecoder::default(),
            started: false,
            done: false,
            content: false,
            finish: false,
            pending: Vec::new(),
            pending_bytes: 0,
        }
    }
    fn push(&mut self, bytes: &[u8], model: &str) -> Result<()> {
        for data in self.decoder.push(bytes)? {
            if self.done {
                break;
            }
            if data.trim().is_empty() {
                continue;
            }
            if data.trim() == "[DONE]" {
                if !self.finish {
                    return Err(Error::http(502, "ZCode 未返回有效结束原因"));
                }
                self.flush()?;
                sdk::stream_bytes(b"data: [DONE]\n\n")?;
                self.done = true;
                break;
            }
            let mut value: Value = serde_json::from_str(&data)
                .map_err(|_| Error::http(502, "ZCode SSE 事件格式无效"))?;
            if errors::has_error(&value) {
                let status = value["error"]["status"]
                    .as_u64()
                    .or_else(|| value["error"]["statusCode"].as_u64())
                    .and_then(|value| u16::try_from(value).ok())
                    .unwrap_or(502);
                if errors::code(&value) == Some(3007) {
                    captcha::challenge()?;
                }
                return Err(errors::failure(status, &value));
            }
            let choices = value["choices"]
                .as_array()
                .ok_or_else(|| Error::http(502, "ZCode SSE 事件缺少 choices"))?;
            for choice in choices {
                self.content |= ["content", "reasoning_content", "reasoning"]
                    .iter()
                    .any(|key| {
                        choice["delta"][*key]
                            .as_str()
                            .is_some_and(|value| !value.is_empty())
                    })
                    || choice["delta"]["tool_calls"]
                        .as_array()
                        .is_some_and(|tools| !tools.is_empty());
                self.finish |= choice["finish_reason"]
                    .as_str()
                    .is_some_and(|reason| !reason.is_empty());
            }
            value["model"] = json!(model);
            if self.started {
                sdk::stream_event(&value)?;
            } else {
                self.pending_bytes = self.pending_bytes.saturating_add(data.len());
                if self.pending.len() >= 128 || self.pending_bytes > 1024 * 1024 {
                    return Err(Error::http(502, "ZCode 仅返回控制帧，没有有效内容"));
                }
                self.pending.push(value);
                if self.content || self.finish {
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

pub fn forward(mut params: Value) -> Result<()> {
    let record = account::record()?;
    let secret = &record["secret"];
    let client_headers = params
        .as_object_mut()
        .and_then(|object| object.remove("headers"))
        .unwrap_or(Value::Null);
    let mut original = sdk::take_body(&mut params).unwrap_or(Value::Null);
    drop(params);
    if !original.is_object() {
        return Err(Error::http(400, "ZCode 请求体必须是对象"));
    }
    let model = original["model"]
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| Error::http(400, "ZCode 需要明确的模型名"))?
        .to_string();
    original["stream"] = json!(true);
    let version = config::app_version()?;
    let start_plan = account::plan(secret) == account::START;
    let (url, mut headers, body, wire_model) = if start_plan {
        let token = account::jwt(secret);
        if token.is_empty() {
            return Err(Error::http(400, "套餐池通道缺少 JWT，请核对账号凭据"));
        }
        let wire_model = model.to_ascii_lowercase();
        let mut payload = protocol::request("anthropic", &original, &wire_model)?;
        reasoning::anthropic(&mut payload, &original, &wire_model);
        let context =
            request_context::Context::new(&client_headers, &original, &account::mid(secret))?;
        drop(original);
        prompt::apply(&mut payload, &wire_model, &context)?;
        let mut headers = config::identity_headers(&version, Some("ai-sdk/anthropic/3.0.81"));
        headers["Content-Type"] = json!("application/json");
        headers["Authorization"] = json!(format!("Bearer {token}"));
        headers["anthropic-version"] = json!("2023-06-01");
        if let Some(object) = headers.as_object_mut() {
            object.extend(
                context
                    .headers
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
        }
        (
            format!("{}/api/v1/zcode-plan/anthropic/v1/messages", network::ZCODE),
            headers,
            payload,
            wire_model,
        )
    } else {
        let token = account::access(secret);
        if token.is_empty() {
            return Err(Error::http(400, "编码套餐通道缺少访问凭据"));
        }
        let mut headers = config::identity_headers(&version, None);
        headers["Content-Type"] = json!("application/json");
        headers["Accept"] = json!("*/*");
        headers["Authorization"] = json!(format!("Bearer {token}"));
        reasoning::chat(&mut original, &model);
        if original.get("stream_options").is_none() {
            original["stream_options"] = json!({});
        }
        if let Some(options) = original["stream_options"].as_object_mut() {
            options.entry("include_usage").or_insert(json!(true));
        }
        (config::coding_url(), headers, original, model.clone())
    };
    let url = config::mapped_url(&url, &record)?;
    let balance = if start_plan {
        quota::snapshot(&record)?
    } else {
        None
    };
    account::require_current(&record)?;
    if start_plan {
        let settings = captcha::configuration(false)?;
        if captcha::required(&settings)? {
            let Some((param, region)) = captcha::take(&settings)? else {
                return Err(Error::coded("套餐池请求需要官方人机验证，请打开管理界面为当前账号补充验证；不是额度或凭据失效", "local_precondition"));
            };
            headers["x-aliyun-captcha-verify-param"] = json!(param);
            if !region.is_empty() {
                headers["x-aliyun-captcha-verify-region"] = json!(region);
            }
        }
    }
    account::require_current(&record)?;
    let mut response = http::open("POST", &url, headers, Some(&body))?;
    drop(body);
    account::require_current(&record)?;
    let content_type = response.headers["content-type"]
        .as_str()
        .unwrap_or("")
        .to_ascii_lowercase();
    if !(200..300).contains(&response.status)
        || content_type.split(';').next().unwrap_or("").trim() == "application/json"
    {
        let status = response.status;
        let bytes = response.bytes(1024 * 1024)?;
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if errors::code(&value) == Some(3007) {
            captcha::challenge()?;
        }
        if errors::version_refused(status, &value) {
            return Err(errors::version_refused_error(Some(&version)));
        }
        return Err(quota::failure(
            status,
            &value,
            &record,
            &model,
            balance.as_ref(),
        )?);
    }
    if content_type.contains("text/html") {
        return Err(Error::http(502, "ZCode 返回网页而非流式模型结果"));
    }
    let mut converter = if start_plan {
        Some(protocol::Converter::new("anthropic", &wire_model)?)
    } else {
        None
    };
    let mut upstream_events = SseDecoder::default();
    let mut upstream_complete = false;
    let mut output = Output::new();
    let result = (|| -> Result<()> {
        while let Some(bytes) = response.read()? {
            account::require_current(&record)?;
            if let Some(converter) = &mut converter {
                // 通用 Anthropic 转换器的错误帧不保留业务码；先检查本家原始 error，
                // 其余帧交给共享转换器，工具/思考/usage 不在此重写一套。
                for data in upstream_events.push(&bytes)? {
                    if data.trim().is_empty() || data.trim() == "[DONE]" {
                        continue;
                    }
                    let event: Value = serde_json::from_str(&data)
                        .map_err(|_| Error::http(502, "ZCode 套餐池事件不是有效 JSON"))?;
                    if errors::has_error(&event) {
                        let status = event["error"]["status"]
                            .as_u64()
                            .or_else(|| event["error"]["statusCode"].as_u64())
                            .or_else(|| event["status"].as_u64())
                            .and_then(|value| u16::try_from(value).ok())
                            .unwrap_or(502);
                        if errors::code(&event) == Some(3007) {
                            captcha::challenge()?;
                        }
                        return Err(quota::failure(
                            status,
                            &event,
                            &record,
                            &model,
                            balance.as_ref(),
                        )?);
                    }
                    if event["type"] == "message_stop" {
                        upstream_complete = true;
                    }
                    let converted = converter.push(format!("data: {data}\n\n").as_bytes())?;
                    output.push(&converted, &model)?;
                    if output.done {
                        break;
                    }
                }
            } else {
                output.push(&bytes, &model)?;
            }
            if output.done {
                return Ok(());
            }
        }
        account::require_current(&record)?;
        if let Some(converter) = &mut converter {
            if !upstream_complete {
                return Err(
                    Error::http(502, "ZCode 套餐池流未收到 message_stop，未伪造完成帧")
                        .with_code("runtime_stream_interrupted"),
                );
            }
            output.push(&converter.finish()?, &model)?;
        }
        if !output.done {
            return Err(Error::http(502, "ZCode 上游流提前中断，未收到完整结束标记")
                .with_code("runtime_stream_interrupted"));
        }
        Ok(())
    })();
    if let Err(error) = result {
        if output.started && !output.done {
            sdk::stream_event(&errors::event(&error))?;
        }
        return Err(error);
    }
    Ok(())
}
