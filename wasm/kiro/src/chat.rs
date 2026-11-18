//! AWS二进制流转标准Chat SSE。首个可交付事件前错误仍是调用失败供宿主选路。
//! 开流后只发标准错误并返回真实失败，不二次发head、不伪造mark_limited RPC。

use crate::{
    catalog, client,
    credentials::Credentials,
    endpoints, errors,
    events::{Decoder, Event},
    network, refresh, request, tools, util,
};
use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};

fn ask(c: &Credentials, body: &Value) -> Result<http::Response> {
    let url = format!(
        "{}/generateAssistantResponse",
        endpoints::runtime(&c.runtime_region()?)?
    );
    let user_agent = client::streaming_agent()?;
    let mut headers = network::auth(c);
    headers["Content-Type"] = json!("application/json");
    headers["Accept"] = json!("application/vnd.amazon.eventstream");
    headers["x-amzn-codewhisperer-optout"] = json!("true");
    headers["x-amzn-kiro-agent-mode"] = json!("vibe");
    headers["amz-sdk-invocation-id"] = json!(util::uuid()?);
    headers["amz-sdk-request"] = json!("attempt=1; max=1");
    headers["x-amz-user-agent"] = json!(user_agent);
    headers["User-Agent"] = json!(user_agent);
    let response = http::open("POST", &url, headers, Some(body)).map_err(errors::transport)?;
    if !(200..300).contains(&response.status) {
        return match network::read(response, &c.secrets(), 2 * 1024 * 1024) {
            Err(error) => Err(error),
            Ok(_) => Err(Error::http(502, "Kiro返回了异常状态")),
        };
    }
    let mime = network::header(&response.headers, "content-type").to_ascii_lowercase();
    if !mime.is_empty()
        && !mime.contains("application/vnd.amazon.eventstream")
        && !mime.contains("application/octet-stream")
    {
        return Err(Error::http(502, "Kiro推理未返回AWS二进制事件流"));
    }
    Ok(response)
}
struct Output {
    id: String,
    model: String,
    created: i64,
}
impl Output {
    fn value(&self, delta: Value, finish: Option<&str>) -> Value {
        json!({ "id": self.id, "object": "chat.completion.chunk", "created": self.created,
            "model": self.model, "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }] })
    }
    fn delta(&self, delta: Value) -> Result<()> {
        sdk::stream_event(&self.value(delta, None))
    }
    fn failure(&self, error: Error, usage: Option<Value>) -> Result<()> {
        if errors::host_stop(&error) {
            return Err(error);
        }
        if let Some(usage) = usage {
            let mut value = self.value(json!({}), None);
            value["choices"] = json!([]);
            value["usage"] = usage;
            sdk::stream_event(&value)?;
        }
        sdk::stream_event(&errors::event(&error))?;
        sdk::stream_bytes(b"data: [DONE]\n\n")?;
        Err(error)
    }
}
pub fn forward(params: &Value) -> Result<()> {
    let input = &params["body"];
    let model = params["wire_model"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| input["model"].as_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("auto");
    let names = tools::restore(input)?;
    let mut session = refresh::ready()?;
    let record = match catalog::for_chat(&session.credentials, model) {
        Err(error) if error.status == Some(401) && !session.credentials.api_key() => {
            session = refresh::after_rejection(&session.credentials)?;
            catalog::for_chat(&session.credentials, model)?
        }
        result => result?,
    };
    let profile = if session.credentials.api_key() {
        ""
    } else {
        &session.credentials.profile
    };
    let mut built = request::build(input, model, profile, &record)?;
    let mut retried = false;
    let (mut decoder, first) = loop {
        let attempt = (|| -> Result<(Decoder, Option<Event>)> {
            let response = ask(&session.credentials, &built.body)?;
            let mut decoder = Decoder::new(response, built.split_thinking, names.clone());
            let first = decoder.next(&session.credentials.secrets())?;
            Ok((decoder, first))
        })();
        match attempt {
            Err(error)
                if error.status == Some(401) && !retried && !session.credentials.api_key() =>
            {
                retried = true;
                session = match refresh::after_rejection(&session.credentials) {
                    Ok(session) => session,
                    Err(refresh_error) if errors::host_stop(&refresh_error) => {
                        return Err(refresh_error)
                    }
                    Err(_) => return Err(error),
                };
                if session.credentials.profile.is_empty() {
                    if let Some(body) = built.body.as_object_mut() {
                        body.remove("profileArn");
                    }
                } else {
                    built.body["profileArn"] = json!(session.credentials.profile);
                }
            }
            result => break result?,
        }
    };
    let first = first.ok_or_else(|| Error::http(502, "Kiro没有返回任何可交付事件"))?;
    let output = Output {
        id: format!("chatcmpl-{}", util::message_id()?),
        model: model.into(),
        created: util::now()? / 1000,
    };
    sdk::stream_head(200)?;
    output.delta(json!({ "role": "assistant", "content": "" }))?;
    let mut pending = Some(first);
    loop {
        let event = match pending.take() {
            Some(event) => event,
            None => match decoder.next(&session.credentials.secrets()) {
                Ok(Some(event)) => event,
                Ok(None) => {
                    return output
                        .failure(Error::http(502, "Kiro事件流未返回终态"), decoder.usage())
                }
                Err(error) => return output.failure(error, decoder.usage()),
            },
        };
        match event {
            Event::Keepalive => sdk::stream_bytes(b": keepalive\n\n")?,
            Event::Delta(delta) => output.delta(delta)?,
            Event::Stop {
                reason,
                usage,
                context,
            } => {
                let mut value = output.value(json!({}), Some(&reason));
                if let Some(usage) = usage {
                    value["usage"] = usage;
                }
                if let Some(context) = context {
                    value["kiro_context_usage_percentage"] = json!(context);
                }
                sdk::stream_event(&value)?;
                sdk::stream_bytes(b"data: [DONE]\n\n")?;
                return Ok(());
            }
        }
    }
}
