//! 每请求新会话：sendmsg -> idallochstr -> sessionswitch -> getchatcontent。
//! 不重放不确定sendmsg（防重复任务）；首帧前错误交宿主选路，流中错误保持失败。

use crate::{
    catalog, endpoints, errors, network, request, session,
    stream::{Event, Reader},
    util,
};
use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};

struct Output {
    id: String,
    model: String,
    created: i64,
}
impl Output {
    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        json!({ "id": self.id, "object": "chat.completion.chunk", "created": self.created,
            "model": self.model, "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }] })
    }
}
pub fn forward(params: &Value) -> Result<()> {
    let input = &params["body"];
    let prompt = request::prompt(input)?;
    let requested = params["wire_model"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| input["model"].as_str())
        .unwrap_or("auto");
    let ready = session::ready(false)?;
    let (model, warning) = catalog::for_chat(&ready.credentials, requested)?;
    let query = format!("{}{}", endpoints::WEB_QUERY, ready.triple.query());
    let mut secrets = ready.credentials.secrets();
    secrets.push(&ready.triple.bdstoken);
    secrets.push(&ready.triple.uinfo);
    let create = request::create(&prompt, &model, &ready.triple)?;
    session::unchanged(&ready.credentials)?;
    let response = network::post(
        &format!("{}{}?{query}", endpoints::BASE, endpoints::SEND),
        network::headers(&ready.credentials),
        &create,
        &secrets,
    )
    .map_err(errors::uncertain)?;
    errors::status(&response, &secrets).map_err(errors::uncertain)?;
    session::unchanged(&ready.credentials).map_err(errors::uncertain)?;
    let sid = response["data"]["session_id"]
        .as_str()
        .filter(|id| util::identifier(id, 512))
        .ok_or_else(|| errors::uncertain(Error::http(502, "库库创建会话未返回session_id")))?;
    let reply = response["data"]["reply_id"].as_str().unwrap_or("");
    if reply.len() > 512 || reply.chars().any(char::is_control) {
        return Err(errors::uncertain(Error::http(502, "库库reply_id格式无效")));
    }
    for (path, body) in [
        (
            endpoints::ALLOCATE,
            json!({ "channel": endpoints::CHANNEL_NAME, "gen_type": 2 }),
        ),
        (
            endpoints::SWITCH,
            json!({ "session_id": sid, "op": 1, "full_access_enabled": 0 }),
        ),
    ] {
        session::unchanged(&ready.credentials).map_err(errors::uncertain)?;
        match network::post(
            &format!("{}{path}?{query}", endpoints::BASE),
            network::headers(&ready.credentials),
            &body,
            &secrets,
        ) {
            Err(error) if errors::host_stop(&error) || error.status == Some(401) => {
                return Err(errors::uncertain(error))
            }
            _ => {} // 原生客户端的辅助步骤容错；最终getchatcontent仍须真实成功。
        }
    }
    let payload = request::generate(&prompt, &model, sid, reply);
    session::unchanged(&ready.credentials).map_err(errors::uncertain)?;
    let mut headers = network::headers(&ready.credentials);
    headers["Accept"] = json!("text/event-stream");
    let response = http::open(
        "POST",
        &format!("{}{}?{query}", endpoints::BASE, endpoints::STREAM),
        headers,
        Some(&payload),
    )
    .map_err(|error| errors::uncertain(errors::transport(error)))?;
    if !(200..300).contains(&response.status) {
        return match network::read(response, &secrets, 1024 * 1024) {
            Err(error) => Err(errors::uncertain(error)),
            Ok(_) => Err(errors::uncertain(Error::http(502, "库库对话响应异常"))),
        };
    }
    let mime = network::header(&response.headers, "content-type").to_ascii_lowercase();
    if !mime.is_empty() && !mime.contains("text/event-stream") {
        return Err(errors::uncertain(Error::http(
            502,
            "库库对话未返回SSE事件流",
        )));
    }
    let mut reader = Reader::new(response);
    let first = reader
        .next(&secrets)
        .map_err(errors::uncertain)?
        .ok_or_else(|| errors::uncertain(Error::http(502, "库库对话流为空")))?;
    let output = Output {
        id: format!("chatcmpl-{}", util::random_hex(16)?),
        model,
        created: util::now()? / 1000,
    };
    sdk::stream_head(200)?;
    let mut head = output.chunk(json!({ "role": "assistant", "content": "" }), None);
    if let Some(warning) = warning {
        head["kuku_warning"] = json!(warning);
    }
    sdk::stream_event(&head)?;
    let mut pending = Some(first);
    let mut usage = serde_json::Map::new();
    loop {
        session::unchanged(&ready.credentials).map_err(errors::uncertain)?;
        let event = match pending.take() {
            Some(event) => event,
            None => match reader.next(&secrets) {
                Ok(Some(event)) => event,
                Ok(None) => return Err(errors::uncertain(Error::http(502, "库库流没有有效终态"))),
                Err(error) => {
                    let error = errors::uncertain(error);
                    if !errors::host_stop(&error) {
                        sdk::stream_event(&errors::event(&error))?;
                        sdk::stream_bytes(b"data: [DONE]\n\n")?;
                    }
                    return Err(error);
                }
            },
        };
        match event {
            Event::Text(text) => {
                sdk::stream_event(&output.chunk(json!({ "content": text }), None))?
            }
            Event::Reasoning(text) => {
                sdk::stream_event(&output.chunk(json!({"reasoning_content":text}), None))?;
            }
            Event::Usage(value) => {
                if let Some(fields) = value.as_object() {
                    usage.extend(fields.clone());
                }
                let mut chunk = output.chunk(json!({}), None);
                chunk["choices"] = json!([]);
                chunk["usage"] = Value::Object(usage.clone());
                sdk::stream_event(&chunk)?;
            }
            Event::Done => {
                // 只留本次正文已完成的本地证据，签到时才可上报 CHAT；不自动消费或领奖。
                match crate::checkin::record_chat(&ready.credentials) {
                    Err(error) if errors::host_stop(&error) => return Err(error),
                    _ => {}
                }
                sdk::stream_event(&output.chunk(json!({}), Some("stop")))?;
                sdk::stream_bytes(b"data: [DONE]\n\n")?;
                return Ok(());
            }
        }
    }
}
