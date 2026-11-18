//! 主动标准Chat SSE；协议转换由SDK持有，必须真实DONE，完整保留工具/思考/usage。
//! 流开始后的额度换号和错误元数据传递仍依赖宿主，包内不能回滚已下发文本。
use super::{catalog, errors, headers, network, normalize, reasoning, session};
use agent2api_wasm_sdk::{
    self as sdk, json, protocol::Converter, sse::SseDecoder, Error, Result, Value,
};

pub fn forward(params: &Value) -> Result<()> {
    let current = session::ensure(false)?;
    let model = params["body"]["model"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("auto");
    let declared = catalog::check(&current, model)?;
    if declared.as_ref().is_some_and(|row| row["kind"] == "image") {
        return Err(Error::http(400, "请通过图片接口使用该模型"));
    }
    let (mut body, report) = normalize::normalize_outbound(&params["body"], &current.secret);
    if !body.is_object() {
        return Err(Error::http(400, "WorkBuddy请求体必须为对象"));
    }
    body["model"] = json!(model);
    body["stream"] = json!(true);
    let _ = (report.notable(), report.describe());
    reasoning::normalize_outbound(&mut body, declared.as_ref());
    let messages = body["messages"]
        .as_array_mut()
        .filter(|messages| !messages.is_empty())
        .ok_or_else(|| Error::http(400, "WorkBuddy请求必须包含非空messages数组"))?;
    if messages.iter().any(|message| !message.is_object()) {
        return Err(Error::http(400, "WorkBuddy消息必须为对象"));
    }
    if !messages[0]["role"]
        .as_str()
        .is_some_and(|role| role.eq_ignore_ascii_case("system"))
    {
        messages.insert(
            0,
            json!({ "role": "system", "content": "你是一个得力助手" }),
        );
    }
    let mut response = network::open(
        "POST",
        &current.endpoint()?,
        "/v2/chat/completions",
        headers::conversation(&current.secret, params)?,
        Some(&body),
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
    let mut converter = Converter::new("chat", model)?;
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
            "credentialScope": error.credential_scope, "type": "upstream_error" } }),
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
