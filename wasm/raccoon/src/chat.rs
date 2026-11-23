//! 主动输出标准Chat SSE，保留工具/思考/图片字段并改回请求模型名。
//! HTTP200错误信封及流内业务错误都先分类；必须真实DONE，不把截断流伪装成成功。

use super::{config, conversation, endpoints, errors, iam, network, reasoning, session, stream};
use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};

fn is_json_response(response: &http::Response) -> bool {
    response.header_values("content-type").iter().any(|value| {
        let mime = value
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        mime == "application/json" || mime.ends_with("+json")
    })
}

fn open_stream(session: &session::Session, headers: Value, body: &Value) -> Result<http::Response> {
    let path = format!("{}/chat/completions", endpoints::LLM);
    let mut confirmed = false;
    let mut headers = Some(headers);
    loop {
        let current_headers = headers.take().ok_or_else(|| Error::new("小浣熊请求头缺失"))?;
        let response = network::open(
            "POST",
            &path,
            &session.credential.token,
            &session.credential.org,
            current_headers.clone(),
            Some(body),
        )?;
        if (200..300).contains(&response.status) && !is_json_response(&response) {
            return Ok(response);
        }
        let reply = network::read(response, 1024 * 1024)?;
        if !confirmed && errors::code(&reply.value) == Some(200_901) {
            confirmed = true;
            iam::confirm(&session.credential.token)?;
            headers = Some(current_headers);
            continue;
        }
        return Err(errors::upstream(
            reply.status,
            &reply.value,
            &[&session.credential.token, &session.credential.refresh],
        ));
    }
}

pub fn forward(params: &Value) -> Result<()> {
    let session = session::ensure(false)?;
    let model = params["body"]["model"]
        .as_str()
        .filter(|model| !model.trim().is_empty())
        .ok_or_else(|| Error::http(400, "小浣熊请求缺少模型名"))?;
    let mut body = reasoning::normalize(&params["body"], model)?;
    body["stream"] = json!(true);
    if !body["stream_options"].is_object() {
        body["stream_options"] = json!({});
    }
    body["stream_options"]["include_usage"] = json!(true);
    let mut headers = config::chat()?;
    let correlation = conversation::headers(params, &session)?;
    for (key, value) in correlation.as_object().into_iter().flatten() {
        headers[key] = value.clone();
    }
    headers["Accept"] = json!("application/json");
    let mut response = open_stream(&session, headers, &body)?;
    let mut stream = stream::Stream::new();
    let result = (|| {
        while let Some(bytes) = response.read()? {
            if stream.push(&bytes, model, &session.credential.token)? { return Ok(()); }
        }
        stream.finish(model, &session.credential.token)
    })();
    if let Err(mut error) = result {
        error.message = errors::clean(
            &error.message,
            &[&session.credential.token, &session.credential.refresh],
        );
        if stream.started() {
            sdk::stream_event(
                &json!({ "error": { "message": error.message, "status": error.status.unwrap_or(502),
            "code": error.upstream_code, "providerCode": error.code, "type": "upstream_error" } }),
            )?;
        }
        return Err(error);
    }
    Ok(())
}
pub(super) fn emit(event: &str, model: &str, token: &str, started: &mut bool) -> Result<bool> {
    if event.trim() == "[DONE]" {
        if !*started {
            return Err(Error::http(502, "小浣熊没有返回任何对话片段"));
        }
        return Ok(true);
    }
    let mut value: Value =
        serde_json::from_str(event).map_err(|_| Error::http(502, "小浣熊流式JSON无效"))?;
    if errors::failed(200, &value) {
        return Err(errors::upstream(200, &value, &[token]));
    }
    if !value["choices"].is_array() {
        return Err(Error::http(502, "小浣熊流式响应缺少choices"));
    }
    value["model"] = json!(model);
    if !*started {
        sdk::stream_head(200)?;
        *started = true;
    }
    sdk::stream_event(&value)?;
    Ok(false)
}
