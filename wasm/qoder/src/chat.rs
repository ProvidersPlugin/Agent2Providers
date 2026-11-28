//! 国内 COSY 对话。编码后原始字节签名，受控 HTTP 输出标准 Chat SSE。
//! 10605 仅首帧前在预算内轮询队列，放弃或结束时释放占位；105/401 同账号强刷一次；已出流错误绝不伪造成功。
use super::{
    account, client, config, cosy, endpoints, errors, models,
    output::{self, Event, Output},
    protocol, queue, refresh, util,
};
use agent2api_wasm_sdk::{self as sdk, http, json, sse::SseDecoder, Error, Result, Value};

const MAX_ENCODED: usize = 16 * 1024 * 1024;
struct Failure {
    error: Error,
    started: bool,
    queue: Option<errors::QueueInfo>,
}

pub fn forward(params: &Value) -> Result<()> {
    let body = &params["body"];
    if !body.is_object() {
        return Err(Error::http(400, "Qoder 请求体必须是 JSON 对象"));
    }
    if http::json_exceeds(body, MAX_ENCODED / 4 * 3)? {
        return Err(Error::http(
            413,
            "Qoder 输入超过当前受控传输容量，请缩小图片或上下文；未丢弃内容",
        )
        .with_code("plugin_request_too_large"));
    }
    let model_name = body["model"].as_str().unwrap_or("");
    let max_waits = config::queue_waits()?;
    let budget_ms = config::queue_budget()?;
    let mut snapshot = refresh::ensure(false)?;
    let mut model = models::resolve(model_name, &snapshot)?;
    let mut renewed = false;
    let mut waited = 0_u64;
    let mut queue_deadline: Option<i64> = None;
    let mut active_lease: Option<queue::Lease> = None;
    let mut resend_ids: Option<(String, String)> = None;
    loop {
        account::unchanged(&snapshot)?;
        let reuse = resend_ids
            .as_ref()
            .map(|(set_id, sess_id)| (set_id.as_str(), sess_id.as_str()));
        let payload = protocol::build(
            body,
            &model,
            &snapshot.credentials,
            params["session"].as_str(),
            reuse,
        )?;
        let thinking = payload.thinking;
        let request_id = payload.request_id.clone();
        let request_set_id = payload.request_set_id.clone();
        let session_id = payload.session_id.clone();
        let model_key = model["upstreamKey"].as_str().unwrap_or("").to_string();
        let raw = serde_json::to_vec(&payload.value)
            .map_err(|_| Error::http(400, "Qoder 请求体无法编码"))?;
        drop(payload);
        if raw.len() > MAX_ENCODED / 4 * 3 {
            if let Some(lease) = active_lease.take() {
                queue::finish(&lease, &snapshot.credentials);
            }
            return Err(Error::http(
                413,
                "Qoder 编码请求超过当前受控传输容量，请缩小图片或上下文；未丢弃内容",
            )
            .with_code("plugin_request_too_large"));
        }
        let encoded = cosy::encode(&raw);
        drop(raw);
        let mut headers = cosy::headers(Some(&encoded), endpoints::CHAT, &snapshot.credentials)?;
        headers["Content-Type"] = json!("application/json");
        headers["Accept"] = json!("text/event-stream");
        headers["Cache-Control"] = json!("no-cache");
        headers["Accept-Encoding"] = json!("identity");
        headers["X-Model-Key"] = model["upstreamKey"].clone();
        headers["X-Model-Source"] = model["config"]["source"].clone();
        account::unchanged(&snapshot)?;
        let response = http::open_raw(
            "POST",
            &client::url(endpoints::GATEWAY, endpoints::CHAT)?,
            headers,
            Some(&encoded),
        );
        drop(encoded);
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                if let Some(lease) = active_lease.take() {
                    queue::finish(&lease, &snapshot.credentials);
                }
                return Err(error);
            }
        };
        match consume(response, model_name, thinking, &snapshot) {
            Ok(()) => {
                if let Some(lease) = active_lease.take() {
                    queue::finish(&lease, &snapshot.credentials);
                }
                return Ok(());
            }
            Err(Failure {
                error,
                started: true,
                ..
            }) => {
                if let Some(lease) = active_lease.take() {
                    queue::finish(&lease, &snapshot.credentials);
                }
                return Err(error);
            }
            Err(Failure {
                error,
                started: false,
                queue: queued_info,
            }) => {
                if error.code.as_deref() == Some("qoder_model_busy") {
                    let info = queued_info.unwrap_or_default();
                    let lease = active_lease.get_or_insert_with(|| queue::Lease {
                        origin: endpoints::GATEWAY.to_string(),
                        model_key: info
                            .model_key
                            .clone()
                            .filter(|value| !value.is_empty())
                            .unwrap_or_else(|| model_key.clone()),
                        request_set_id: request_set_id.clone(),
                        session_id: session_id.clone(),
                        request_id: request_id.clone(),
                        queue_type: info.queue_type.clone(),
                        began: util::now().unwrap_or(0),
                    });
                    lease.request_id = request_id;
                    if info.queue_type.is_some() {
                        lease.queue_type = info.queue_type.clone();
                    }
                    if max_waits > 0 && budget_ms > 0 && waited < max_waits {
                        let deadline = match queue_deadline {
                            Some(deadline) => deadline,
                            None => {
                                let value = util::now()?.saturating_add(budget_ms);
                                queue_deadline = Some(value);
                                value
                            }
                        };
                        match queue::wait(lease, &info, &snapshot.credentials, &snapshot, deadline)
                        {
                            Ok((true, _)) => {
                                waited += 1;
                                resend_ids =
                                    Some((lease.request_set_id.clone(), lease.session_id.clone()));
                                continue;
                            }
                            Ok((false, latest)) => {
                                if let Some(lease) = active_lease.take() {
                                    queue::finish(&lease, &snapshot.credentials);
                                }
                                let note = (waited > 0).then_some("队列放行后重发仍被排队");
                                return Err(errors::queued_failure(&latest, note));
                            }
                            Err(wait_error) => {
                                if let Some(lease) = active_lease.take() {
                                    queue::finish(&lease, &snapshot.credentials);
                                }
                                return Err(wait_error);
                            }
                        }
                    }
                    if let Some(lease) = active_lease.take() {
                        queue::finish(&lease, &snapshot.credentials);
                    }
                    if waited > 0 {
                        return Err(errors::queued_failure(&info, Some("队列放行后重发仍被排队")));
                    }
                    return Err(error);
                }
                if let Some(lease) = active_lease.take() {
                    queue::finish(&lease, &snapshot.credentials);
                }
                if error.code.as_deref() == Some("qoder_auth_expired")
                    && !renewed
                    && snapshot.credentials.can_refresh()
                {
                    renewed = true;
                    resend_ids = None;
                    account::unchanged(&snapshot)?;
                    let original = snapshot.credentials.binding();
                    snapshot = refresh::ensure(true)?;
                    if snapshot.credentials.binding() != original {
                        return Err(account::changed());
                    }
                    model = models::resolve(model_name, &snapshot)?;
                    continue;
                }
                return Err(error);
            }
        }
    }
}

fn consume(
    mut response: http::Response,
    model: &str,
    thinking: bool,
    snapshot: &account::Snapshot,
) -> std::result::Result<(), Failure> {
    let mut output = Output::new(model, thinking).map_err(|error| Failure {
        error,
        started: false,
        queue: None,
    })?;
    let status = response.status;
    let content_type = response.headers["content-type"]
        .as_str()
        .unwrap_or("")
        .to_ascii_lowercase();
    if !(200..300).contains(&status)
        || content_type.contains("json")
        || content_type.contains("html")
    {
        let bytes = response.bytes(1024 * 1024).map_err(|error| Failure {
            error,
            started: false,
            queue: None,
        })?;
        let value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            json!(String::from_utf8_lossy(&bytes)
                .chars()
                .take(16 * 1024)
                .collect::<String>())
        });
        return Err(Failure {
            error: errors::failure(status, &value),
            started: false,
            queue: errors::queue_info(&value),
        });
    }
    let mut decoder = SseDecoder::default();
    let mut queue = None;
    let result = (|| -> Result<()> {
        while let Some(bytes) = response.read()? {
            account::unchanged(snapshot)?;
            for data in decoder.push(&bytes)? {
                match output::parse(&data) {
                    Ok(Event::Skip) => {}
                    Ok(Event::Done) => return output.finish(),
                    Ok(Event::Chunk(chunk)) => output.consume(chunk)?,
                    Err(error) => {
                        if error.code.as_deref() == Some("qoder_model_busy") {
                            if let Ok(value) = serde_json::from_str::<Value>(&data) {
                                queue = errors::queue_info(&value);
                            }
                        }
                        return Err(error);
                    }
                }
            }
        }
        Err(Error::http(502, "Qoder 上游流提前中断，未收到完成标记")
            .with_code("runtime_stream_interrupted"))
    })();
    if let Err(error) = result {
        if output.started {
            if let Err(send) = sdk::stream_event(&errors::event(&error)) {
                return Err(Failure {
                    error: send,
                    started: true,
                    queue: None,
                });
            }
        }
        return Err(Failure {
            error,
            started: output.started,
            queue,
        });
    }
    Ok(())
}
