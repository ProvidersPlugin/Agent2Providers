//! 预读真实 ADK 输出后才发成功 head；明确鉴权失败只在未输出时续期重试一次。
//! 所有 HTTP/转换句柄随调用回收；不用线程，不假定有跨调用的上游会话。

use crate::{
    adk::{self, Adk},
    catalog,
    credentials::Credentials,
    endpoints, errors, network, refresh, request,
    sse::Reader,
    util,
};
use agent2api_wasm_sdk::{self as sdk, http, protocol::Converter, Error, Result, Value};

fn classify_http(response: http::Response, credential: &Credentials) -> Result<Reader> {
    let response_status = response.status;
    let mime = network::header(&response.headers, "content-type").to_ascii_lowercase();
    if mime.contains("text/html")
        || mime.contains("application/xhtml+xml")
        || network::header(&response.headers, "bxpunish") == "1"
    {
        return Err(Error::http(
            403,
            "Accio 返回网页验证或风控拦截，请在官方客户端确认账号状态",
        )
        .with_code("accio_challenge_required"));
    }
    if !(200..300).contains(&response.status) || mime.contains("application/json") {
        let bytes = response.bytes(1024 * 1024).map_err(errors::transport)?;
        let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        return Err(errors::upstream(
            response_status,
            &errors::code(&parsed),
            &errors::message(&parsed),
            &credential.secrets(),
        ));
    }
    // 原生也以实际 data 帧判定，不仅依赖 Content-Type；部分上游漏报正确 MIME。
    Ok(Reader::new(response))
}

fn value(
    reader: &mut Reader,
    session: &refresh::Session,
    adapter: &mut Adk,
) -> Result<Option<Value>> {
    let credential = &session.credentials;
    loop {
        let event = reader.next()?;
        refresh::check(session)?;
        let Some(event) = event else {
            return Ok(None);
        };
        if event.name == "error" {
            let value = serde_json::from_str::<Value>(&event.data).unwrap_or(Value::Null);
            return Err(errors::upstream(
                200,
                &errors::code(&value),
                &errors::message(&value),
                &credential.secrets(),
            ));
        }
        if event.data.trim().eq_ignore_ascii_case("[DONE]") {
            // 上游真实 DONE 可作为 ADK 完成依据，薄适配随后转换成 Gemini finishReason。
            adapter.terminal = true;
            return Ok(None);
        }
        if event.data.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&event.data).map_err(|_| {
            Error::http(502, "Accio 对话事件不是有效 JSON").with_code("accio_non_sse_response")
        })?;
        if !value.is_object() {
            return Err(Error::http(502, "Accio 对话事件不是对象"));
        }
        if adk::version_notice(&value) {
            return Err(Error::http(
                502,
                "Accio 拒绝了客户端版本，请更新插件，未把升级提示当作模型回答",
            )
            .with_code("accio_client_version"));
        }
        return Ok(Some(value));
    }
}

struct Opening {
    reader: Reader,
    adapter: Adk,
    events: Vec<Value>,
}

fn open(session: &refresh::Session, body: &Value) -> Result<Opening> {
    refresh::check(session)?;
    let credential = &session.credentials;
    let id = body["request_id"]
        .as_str()
        .ok_or_else(|| Error::http(500, "Accio 请求缺少编号"))?;
    let url = format!(
        "{}{}?sg_k={}",
        endpoints::GATEWAY,
        endpoints::CHAT,
        util::md5(id)
    );
    let response = http::open("POST", &url, network::chat_headers(credential)?, Some(body))
        .map_err(errors::transport)?;
    let mut reader = classify_http(response, credential)?;
    let mut adapter = Adk::default();
    let mut pending = Vec::new();
    let mut size = 0usize;
    while let Some(value) = value(&mut reader, session, &mut adapter)? {
        let events = adapter.consume(&value, credential)?;
        for event in events {
            size = size.saturating_add(event.to_string().len());
            if size > 1024 * 1024 || pending.len() >= 4096 {
                return Err(Error::http(502, "Accio 首帧前的无效内容过多"));
            }
            pending.push(event);
        }
        if adapter.output || adapter.has_named_tool() {
            return Ok(Opening {
                reader,
                adapter,
                events: pending,
            });
        }
        if adapter.terminal {
            break;
        }
    }
    // 只验证空流/具名工具/SAFETY 终态；具体收尾统一在 drive 内做一次。
    adapter.validate_end()?;
    Ok(Opening {
        reader,
        adapter,
        events: pending,
    })
}

fn convert(converter: &mut Converter, value: &Value) -> Result<()> {
    let bytes = converter.push(format!("data: {value}\n\n").as_bytes())?;
    if !bytes.is_empty() {
        sdk::stream_bytes(&bytes)?;
    }
    Ok(())
}

fn without_unknown_usage(bytes: &[u8]) -> Result<Vec<u8>> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| Error::http(502, "Accio 协议转换输出不是 UTF-8"))?;
    let mut output = Vec::new();
    for frame in text.split_inclusive("\n\n") {
        let payload = frame
            .lines()
            .find_map(|line| line.strip_prefix("data:"))
            .map(str::trim);
        let usage_only = payload
            .and_then(|payload| serde_json::from_str::<Value>(payload).ok())
            .is_some_and(|value| {
                value.get("usage").is_some()
                    && value["choices"].as_array().is_some_and(Vec::is_empty)
            });
        if !usage_only {
            output.extend_from_slice(frame.as_bytes());
        }
    }
    Ok(output)
}

fn drive(
    mut opening: Opening,
    current: &refresh::Session,
    model: &str,
    session: &str,
) -> Result<()> {
    let credential = &current.credentials;
    refresh::check(current)?;
    let mut converter = Converter::with_session("gemini", model, session)?;
    sdk::stream_head(200)?;
    let result: Result<()> = (|| {
        for event in opening.events {
            convert(&mut converter, &event)?;
        }
        if !opening.adapter.terminal {
            while let Some(value) = value(&mut opening.reader, current, &mut opening.adapter)? {
                for event in opening.adapter.consume(&value, credential)? {
                    convert(&mut converter, &event)?;
                }
                if opening.adapter.terminal {
                    break;
                }
            }
        }
        refresh::check(current)?;
        for event in opening.adapter.end()? {
            convert(&mut converter, &event)?;
        }
        refresh::check(current)?;
        let final_bytes = converter.finish()?;
        // Gemini 核心在没有 usageMetadata 时会补零值；原生 Accio 不虚构这份读数。
        // 这里只在确实缺失时删掉核心的 usage-only 帧，正文/工具/结束帧逐字保留。
        let final_bytes = if opening.adapter.has_usage() {
            final_bytes
        } else {
            without_unknown_usage(&final_bytes)?
        };
        if !final_bytes.is_empty() {
            sdk::stream_bytes(&final_bytes)?;
        }
        Ok(())
    })();
    if let Err(mut error) = result {
        error.message = errors::redact(&error.message, &credential.secrets());
        // 流中途限额仍缺宿主受控记账通道；只发真实错误，不猜测 RPC 或补成功帧。
        let _ = sdk::stream_event(&errors::event(&error));
        return Err(error);
    }
    Ok(())
}

pub fn forward(params: &Value) -> Result<()> {
    let model = params["wire_model"]
        .as_str()
        .filter(|model| !model.trim().is_empty())
        .or_else(|| params["body"]["model"].as_str())
        .unwrap_or("");
    if model.trim().is_empty() {
        return Err(Error::http(400, "请从 Accio 国际版目录选择一个模型"));
    }
    let session_key = params["session"].as_str().unwrap_or("");
    if session_key.len() > 1024 || session_key.chars().any(char::is_control) {
        return Err(Error::http(400, "Accio 会话标识无效"));
    }
    let mut session = refresh::ready()?;
    let mut retried = false;
    loop {
        let opened = (|| {
            let entry = catalog::for_chat(&session, model)?;
            let body = request::build(&params["body"], &entry, &session.credentials)?;
            open(&session, &body)
        })();
        match opened {
            Ok(opening) => return drive(opening, &session, model, session_key),
            Err(error)
                if error.status == Some(401)
                    && !retried
                    && !session.credentials.refresh.is_empty() =>
            {
                retried = true;
                session = refresh::after_rejection(&session.credentials)?;
            }
            Err(error) => return Err(error),
        }
    }
}
