//! 主动标准Chat SSE；协议转换由SDK持有，必须真实DONE，完整保留工具/思考/usage。
//! 流开始后的错误携带原业务码与窗口，宿主仅对原账号模型记账，不回滚或重放文本。
use super::{catalog, errors, headers, network, normalize, reasoning, session};
use agent2api_wasm_sdk::{
    self as sdk, json, protocol::Converter, sse::SseDecoder, Error, Result, Value,
};

pub fn forward(mut params: Value) -> Result<()> {
    let current = session::ensure(false)?;
    let model = params["body"]["model"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("auto")
        .to_string();
    let declared = catalog::check(&current, &model)?;
    if declared.as_ref().is_some_and(|row| row["kind"] == "image") {
        return Err(Error::http(400, "请通过图片接口使用该模型"));
    }
    // 正文从入站参数里移交出去：整条出站链路只保留这一份，不再复制。
    let mut body = match params.as_object_mut().and_then(|object| object.remove("body")) {
        Some(body) if body.is_object() => body,
        _ => return Err(Error::http(400, "WorkBuddy请求体必须为对象")),
    };
    drop(params);
    let report = normalize::normalize_outbound(&mut body, &current.secret);
    body["model"] = json!(model);
    body["stream"] = json!(true);
    let _ = (report.notable(), report.describe());
    reasoning::normalize_outbound(&mut body, declared.as_ref());
    if let Some(messages) = body["messages"].as_array_mut() {
        if messages.first().is_some_and(|row| {
            !row["role"]
                .as_str()
                .is_some_and(|role| role.eq_ignore_ascii_case("system"))
        }) {
            messages.insert(
                0,
                json!({ "role": "system", "content": "你是一个得力助手" }),
            );
        }
    }
    // 正文走分片资源通道：不再生成整份 JSON 字符串与 base64 信封。
    let mut response = network::open_chat(
        &current.endpoint()?,
        "/v2/chat/completions",
        headers::chat(&current.secret)?,
        &body,
    )?;
    if !(200..300).contains(&response.status)
        || response
            .header_values("content-type")
            .iter()
            .any(|s| s.to_ascii_lowercase().contains("application/json"))
    {
        let reply = network::read(response, 1024 * 1024)?;
        return Err(errors::upstream(
            reply.status,
            &reply.value,
            &current.secrets(),
        ));
    }
    let mut converter = Converter::new("chat", &model)?;
    let mut decoder = SseDecoder::default();
    let mut started = false;
    let mut done = false;
    let result = (|| {
        while let Some(bytes) = response.read()? {
            let output = converter.push(&bytes)?;
            for event in decoder.push(&output)? {
                emit(&event, &current.secrets(), &mut started, &mut done)?;
            }
            if done {
                break;
            }
        }
        let tail = converter.finish()?;
        let mut events = decoder.push(&tail)?;
        events.extend(decoder.push(b"\n\n")?);
        for event in events {
            emit(&event, &current.secrets(), &mut started, &mut done)?;
        }
        if !done || !started {
            return Err(Error::http(502, "WorkBuddy响应未完整结束"));
        }
        sdk::stream_bytes(b"data: [DONE]\n\n")?;
        Ok(())
    })();
    if let Err(mut error) = result {
        error.message = errors::clean(&error.message, &current.secrets());
        if started {
            sdk::stream_event(
                &json!({ "error": { "message": error.message, "status": error.status.unwrap_or(502),
            "code": error.upstream_code, "providerCode": error.code, "resetAt": error.reset_at,
            "type": "upstream_error" } }),
            )?;
        }
        return Err(error);
    }
    Ok(())
}
fn emit(event: &str, secrets: &[&str], started: &mut bool, done: &mut bool) -> Result<()> {
    if *done {
        return Err(Error::http(502, "WorkBuddy结束后出现额外事件"));
    }
    if event.trim() == "[DONE]" {
        *done = true;
        return Ok(());
    }
    let value: Value =
        serde_json::from_str(event).map_err(|_| Error::http(502, "WorkBuddy流式JSON无效"))?;
    if errors::failed(200, &value) {
        return Err(errors::upstream(200, &value, secrets));
    }
    if !value["choices"].is_array() && !value["usage"].is_object() {
        return Err(Error::http(502, "WorkBuddy流缺少choices或usage"));
    }
    if !*started {
        sdk::stream_head(200)?;
        *started = true;
    }
    // 是否剥除纯换行由宿主开关消费sse_strip_newline_chunks，不能在包内无条件吞帧。
    sdk::stream_event(&value)
}
