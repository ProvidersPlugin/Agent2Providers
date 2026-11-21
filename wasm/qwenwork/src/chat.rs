//! 千问办公国内 COSY 对话：只走固定 gateway，所有工具定义与结果经协议传递，不在本机执行。
//! 首帧前有限排队/同账号续期；出流后错误正式返回，不换设备、账号或伪造成功。
use super::{
    account, client, config, cosy, endpoints, errors, models,
    output::{self, Event, Output},
    protocol, refresh, util,
};
use agent2api_wasm_sdk::{self as sdk, http, json, sse::SseDecoder, Error, Result, Value};

const MAX_ENCODED: usize = 16 * 1024 * 1024;
struct Failure {
    error: Error,
    started: bool,
    retry_ms: u64,
}
pub fn forward(params: &Value) -> Result<()> {
    let body = &params["body"];
    if !body.is_object() {
        return Err(Error::http(400, "千问办公请求体必须是 JSON 对象"));
    }
    if http::json_exceeds(body, MAX_ENCODED / 4 * 3)? {
        return Err(Error::http(
            413,
            "千问办公输入超过当前受控传输容量，请缩小内容；未截断消息",
        )
        .with_code("plugin_request_too_large"));
    }
    let model_name = body["model"].as_str().unwrap_or("");
    let mut snapshot = refresh::ensure(false)?;
    let mut model = models::resolve(model_name, &snapshot)?;
    let waits = config::queue_waits()?;
    let deadline = util::now()?.saturating_add(config::queue_budget()?);
    let mut queued = 0;
    let mut renewed = false;
    loop {
        account::unchanged(&snapshot)?;
        let payload = protocol::build(
            body,
            &model,
            &snapshot.credentials,
            params["session"].as_str(),
        )?;
        let thinking = payload.thinking;
        let raw = serde_json::to_vec(&payload.value)
            .map_err(|_| Error::http(400, "千问办公请求体无法编码"))?;
        drop(payload);
        if raw.len() > MAX_ENCODED / 4 * 3 {
            return Err(Error::http(
                413,
                "千问办公编码请求超过当前受控传输容量，请缩小内容；未截断消息",
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
        headers["X-Internal-Model-Key"] = model["upstreamKey"].clone();
        headers["X-Model-Source"] = model["config"]["source"].clone();
        account::unchanged(&snapshot)?;
        let response = http::open_raw(
            "POST",
            &client::url(endpoints::CHAT)?,
            headers,
            Some(&encoded),
        )
        .map_err(|error| client::transport(error, "对话"))?;
        drop(encoded);
        match consume(response, model_name, thinking, &snapshot) {
            Ok(()) => return Ok(()),
            Err(Failure {
                error,
                started: true,
                ..
            }) => return Err(error),
            Err(Failure {
                error,
                started: false,
                retry_ms,
            }) => {
                if error.code.as_deref() == Some("qwenwork_model_busy") && queued < waits {
                    let now = util::now()?;
                    let delay = retry_ms.clamp(5000, 30_000);
                    if now.saturating_add(delay as i64) > deadline {
                        return Err(error);
                    }
                    queued += 1;
                    sdk::sleep(delay)?;
                    continue;
                }
                if error.code.as_deref() == Some("qwenwork_auth_expired")
                    && !renewed
                    && snapshot.credentials.can_refresh()
                {
                    renewed = true;
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
        retry_ms: 15_000,
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
            error: client::transport(error, "对话"),
            started: false,
            retry_ms: 15_000,
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
            retry_ms: errors::queue_wait(&value),
        });
    }
    let mut decoder = SseDecoder::default();
    let mut retry_ms = 15_000;
    let result = (|| -> Result<()> {
        while let Some(bytes) = response
            .read()
            .map_err(|error| client::transport(error, "对话"))?
        {
            account::unchanged(snapshot)?;
            for data in decoder.push(&bytes)? {
                let event = match output::parse(&data) {
                    Ok(event) => event,
                    Err(error) => {
                        if error.code.as_deref() == Some("qwenwork_model_busy") {
                            if let Ok(value) = serde_json::from_str::<Value>(&data) {
                                retry_ms = errors::queue_wait(&value);
                            }
                        }
                        return Err(error);
                    }
                };
                match event {
                    Event::Skip => {}
                    Event::Done => {
                        account::unchanged(snapshot)?;
                        return output.finish();
                    }
                    Event::Chunk(chunk) => output.consume(chunk)?,
                }
            }
        }
        Err(Error::http(502, "千问办公上游流提前中断，未收到完成标记")
            .with_code("runtime_stream_interrupted"))
    })();
    if let Err(error) = result {
        if output.started {
            if let Err(send) = sdk::stream_event(&errors::event(&error)) {
                return Err(Failure {
                    error: send,
                    started: true,
                    retry_ms,
                });
            }
        }
        return Err(Failure {
            error,
            started: output.started,
            retry_ms,
        });
    }
    Ok(())
}
