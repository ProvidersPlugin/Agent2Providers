//! 上游原生 Chat：保留工具、多模态、思考及 usage 字段，不复制协议转换树。
//! max_tokens>=16 追加1024并按DeepSeek/GLM硬上限裁剪；不剥参重试、不自动注入thinking。
//! 必须收到真实 DONE；审核 finish_reason=content_filter 转失败，不把固定话术当成功。

use super::{account, client, credentials::Credentials, errors, util};
use agent2api_wasm_sdk::{self as sdk, json, protocol, sse::SseDecoder, Error, Result, Value};

fn prepare(body: &Value) -> Result<Value> {
    if !body.is_object() {
        return Err(Error::http(400, "OfficeAce 请求体必须是 JSON 对象"));
    }
    let mut body = body.clone();
    let model = body["model"]
        .as_str()
        .filter(|model| !model.trim().is_empty())
        .ok_or_else(|| Error::http(400, "OfficeAce 请求缺少模型名"))?
        .to_ascii_lowercase();
    body["stream"] = json!(true);
    if let Some(requested) = body["max_tokens"].as_i64() {
        if requested >= 16 {
            let raised = requested.saturating_add(1024);
            let raised = if model.contains("deepseek") {
                raised.min(393_216)
            } else if model.contains("glm") {
                raised.min(131_072)
            } else {
                raised
            };
            body["max_tokens"] = json!(raised);
        }
    }
    if body.get("stream_options").is_none_or(Value::is_null) {
        body["stream_options"] = json!({});
    }
    let options = body["stream_options"]
        .as_object_mut()
        .ok_or_else(|| Error::http(400, "OfficeAce stream_options 必须是对象"))?;
    options.entry("include_usage").or_insert(json!(true));
    Ok(body)
}

pub fn forward(params: &Value) -> Result<()> {
    let (record, credential) = account::snapshot()?;
    let body = prepare(&params["body"])?;
    let model = body["model"].as_str().unwrap_or("");
    let mut response = client::model(
        "POST",
        "/v2/chat/completions",
        &credential,
        Some(&body),
        json!({ "accept": "*/*", "Chat-Id": util::random_hex(16)?, "Session-Id": util::random_hex(16)?, "lang": "en" }),
    )?;
    let json_response = response.header_values("content-type").iter().any(|value| {
        let mime = value
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        mime == "application/json" || mime.ends_with("+json")
    });
    if !(200..300).contains(&response.status) || json_response {
        let reply = client::read(response, false)?;
        return Err(errors::model_error(reply.status, &reply.value, &credential));
    }
    let mut converter = protocol::Converter::new("chat", model)?;
    let mut decoder = SseDecoder::default();
    let mut started = false;
    let result = (|| {
        let mut done = false;
        while let Some(bytes) = response.read()? {
            account::same_model(&record)?;
            let output = converter.push(&bytes)?;
            for event in decoder.push(&output)? {
                if done {
                    return Err(Error::http(502, "OfficeAce 结束标记后出现额外事件"));
                }
                done = emit(&event, &credential, &mut started)?;
            }
            if done {
                let tail = converter.finish()?;
                if !tail.is_empty() {
                    return Err(Error::http(502, "OfficeAce 结束事件后返回了额外内容"));
                }
                sdk::stream_bytes(b"data: [DONE]\n\n")?;
                return Ok(());
            }
        }
        // finish只能刷新真实结束事件；通用转换器会拒绝截断，绝不补成功DONE。
        account::same_model(&record)?;
        let tail = converter.finish()?;
        let mut events = decoder.push(&tail)?;
        events.extend(decoder.push(b"\n\n")?);
        for event in events {
            if done {
                return Err(Error::http(502, "OfficeAce 结束标记后出现额外事件"));
            }
            done = emit(&event, &credential, &mut started)?;
        }
        if !done {
            return Err(Error::http(
                502,
                "OfficeAce 响应提前结束，未收到完整结束标记",
            ));
        }
        sdk::stream_bytes(b"data: [DONE]\n\n")?;
        Ok(())
    })();
    if let Err(mut error) = result {
        error.message = errors::clean(&error.message, &credential.secrets());
        if started {
            sdk::stream_event(&json!({ "error": { "message": error.message,
            "status": error.status.unwrap_or(502), "code": error.code, "upstream_code": error.upstream_code,
            "credential_scope": error.credential_scope, "type": "upstream_error" } }))?;
        }
        return Err(error);
    }
    Ok(())
}

fn emit(event: &str, credential: &Credentials, started: &mut bool) -> Result<bool> {
    if event.trim() == "[DONE]" {
        if !*started {
            return Err(Error::http(502, "OfficeAce 未返回任何对话片段"));
        }
        return Ok(true);
    }
    let value: Value =
        serde_json::from_str(event).map_err(|_| Error::http(502, "OfficeAce 流式 JSON 无效"))?;
    if value.get("error").is_some_and(|error| !error.is_null()) || value.get("error_code").is_some()
    {
        let status = value["error"]["status"]
            .as_u64()
            .filter(|status| (400..600).contains(status))
            .unwrap_or(502) as u16;
        return Err(errors::model_error(status, &value, credential));
    }
    let choices = value["choices"]
        .as_array()
        .ok_or_else(|| Error::http(502, "OfficeAce 流式响应缺少 choices"))?;
    if choices
        .iter()
        .any(|choice| choice["finish_reason"] == "content_filter")
    {
        return Err(errors::model_error(
            400,
            &json!({ "providerCode": "officeace_content_filter" }),
            credential,
        ));
    }
    if !*started {
        sdk::stream_head(200)?;
        *started = true;
    }
    sdk::stream_event(&value)?;
    Ok(false)
}
