use agent2api_wasm_sdk::{json, Result, Value};

use crate::{account, endpoints, errors, models, network, request, stream, token, util};

pub fn forward(params: &Value) -> Result<()> {
    let before = account::current()?;
    token::ensure(false)?;
    let current = account::current()?;
    if current.identity != before.identity {
        return Err(errors::identity());
    }
    let id = params["body"]["model"].as_str().unwrap_or("auto");
    let model = models::for_model(&current, id)?;
    let (protocol, body) = request::build(params, &model)?;
    let path = match protocol.as_str() {
        "anthropic" => "/v1/messages",
        "responses" => "/v1/responses",
        _ => "/v1/chat/completions",
    };
    current.unchanged()?;
    let token = current.checked_access()?;
    let mut headers = network::headers(Some(&token));
    headers["user-agent"] = json!(endpoints::CHAT_USER_AGENT);
    headers["accept"] = json!("text/event-stream");
    if model["authentication"] == "x-api-key" {
        if let Some(fields) = headers.as_object_mut() {
            fields.remove("authorization");
        }
        headers["x-api-key"] = json!(token);
    }
    if protocol == "anthropic" {
        headers["anthropic-version"] = json!(model["protocolHeaders"]["anthropic-version"]
            .as_str()
            .unwrap_or("2023-06-01"));
    }
    let response = agent2api_wasm_sdk::http::open_timeout(
        "POST",
        &format!("{}{path}", endpoints::API),
        headers,
        Some(&body),
        600_000,
    )
    .map_err(|error| util::redact(error, &[token.clone()]))?;
    if !(200..300).contains(&response.status) {
        let reply = network::read(response, &[token.clone()])?;
        return Err(errors::upstream(reply.status, &reply.body, &[token]));
    }
    current.unchanged()?;
    if current.secret["source"] == "desktop" && current.access()? != token {
        return Err(errors::changed());
    }
    let mut output = stream::Output::default();
    match stream::consume(response, &protocol, id, &mut output, &[token], &current) {
        Ok(()) => Ok(()),
        Err(error) => {
            output.fail(&error)?;
            Err(error)
        }
    }
}
