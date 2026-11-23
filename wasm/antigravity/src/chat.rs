//! 一次HTTP一次生成。保留stateful=false，凭据拒绝/配额/10秒退避由宿主预算统一处理。
//! 首个真实有效事件前不发成功head，流后异常不补DONE；转换句柄由SDK按调用清理。

use super::{
    account, aliases, argon, catalog_shape, client, credentials,
    endpoints::{self, Plane},
    errors, model_rules, models, outbound, project, refresh,
    signatures::Cache,
    sse::Reader,
    stream::Guard,
    util,
};
use agent2api_wasm_sdk::{self as sdk, http, protocol::Converter, Error, Result, Value};

struct Opening {
    reader: Reader,
    guard: Guard,
    pending: Vec<Value>,
}

fn response(response: http::Response) -> Result<Reader> {
    let mime = response
        .headers
        .as_object()
        .into_iter()
        .flatten()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .and_then(|(_, value)| value.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !(200..300).contains(&response.status)
        || mime.contains("application/json")
        || mime.contains("text/html")
        || mime.contains("application/xhtml")
    {
        let reply = client::read(response, 4 * 1024 * 1024)?;
        if !(200..300).contains(&reply.status) || errors::failed(&reply.body) {
            return Err(errors::upstream(reply.status, &reply.body));
        }
        return Err(Error::http(502, "Antigravity 对话没有返回 SSE 流"));
    }
    Ok(Reader::new(response, 1024 * 1024 - 64))
}
fn open(secret: &Value, envelope: &Value, wire_model: &str) -> Result<Opening> {
    let headers = endpoints::headers(
        &credentials::token(secret)?,
        "",
        Plane::Content,
        catalog_shape::claude(wire_model),
    )?;
    let mut reader = response(client::stream(
        &endpoints::stream(endpoints::DAILY),
        headers,
        envelope,
    )?)?;
    let mut guard = Guard::new();
    let mut pending = Vec::new();
    let mut size = 0usize;
    while let Some(event) = guard.next(&mut reader)? {
        size = size.saturating_add(event.to_string().len());
        if size > 1024 * 1024 || pending.len() >= 4096 {
            return Err(Error::http(502, "Antigravity 首个有效输出前事件过多"));
        }
        pending.push(event);
        if guard.output || guard.terminal {
            return Ok(Opening {
                reader,
                guard,
                pending,
            });
        }
    }
    guard.finish()?;
    Ok(Opening {
        reader,
        guard,
        pending,
    })
}
fn convert(converter: &mut Converter, cache: &mut Cache, value: &Value) -> Result<Vec<u8>> {
    cache.observe(value)?;
    let bytes = converter.push(format!("data: {value}\n\n").as_bytes())?;
    cache.remember(&bytes)?;
    Ok(bytes)
}
fn drive(mut opening: Opening, mut cache: Cache, model: &str, session: &str) -> Result<()> {
    let mut converter = Converter::with_session("gemini", model, session)?;
    let mut initial = Vec::new();
    for event in opening.pending {
        initial.extend(convert(&mut converter, &mut cache, &event)?);
    }
    sdk::stream_head(200)?;
    let result = (|| {
        sdk::stream_bytes(&initial)?;
        while let Some(event) = opening.guard.next(&mut opening.reader)? {
            let bytes = convert(&mut converter, &mut cache, &event)?;
            sdk::stream_bytes(&bytes)?;
        }
        opening.guard.finish()?;
        cache.finish()?;
        let final_bytes = converter.finish()?;
        cache.remember(&final_bytes)?;
        sdk::stream_bytes(&final_bytes)
    })();
    if let Err(error) = &result {
        let _ = sdk::stream_event(&errors::stream(error));
    }
    result
}

pub fn forward(params: &Value) -> Result<()> {
    let requested = params["wire_model"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| params["body"]["model"].as_str())
        .unwrap_or("")
        .trim();
    if requested.is_empty() || requested.len() > 512 || requested.chars().any(char::is_control) {
        return Err(
            Error::http(400, "Antigravity 请求缺少有效模型名称").with_code("local_precondition")
        );
    }
    let effort = model_rules::read_client_level(&params["body"]);
    let wire = argon::resolve_model(requested, effort.as_deref())
        .map(str::to_string)
        .unwrap_or_else(|| aliases::upstream(requested));
    let record = project::ensure(refresh::ensure(false, None)?, true)?;
    let secret = credentials::parse(&record["secret"])?;
    let list = models::for_request(&record, &wire)?;
    let wire = if argon::is_model(&wire) {
        wire
    } else {
        aliases::for_catalog(requested, &list)
    };
    let entry = catalog_shape::entry(&list, &wire)
        .or_else(|| catalog_shape::entry(&catalog_shape::fallback(), requested));
    let mut envelope = outbound::build(
        &params["body"],
        &wire,
        &util::text(&secret, &["projectId"]),
        credentials::enterprise(&secret),
        entry.as_ref(),
    )?;
    let session = envelope["request"]["sessionId"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let caller_session = params["session"].as_str().unwrap_or("");
    if caller_session.len() > 1024 || caller_session.chars().any(char::is_control) {
        return Err(Error::http(400, "Antigravity 会话标识无效").with_code("local_precondition"));
    }
    let cache = Cache::load(
        &session,
        caller_session,
        params["antigravityCaller"].as_str().unwrap_or(""),
        &wire,
        &secret,
    )?;
    cache.fill(&mut envelope);
    account::unchanged(&record["secret"])?;
    let opening = open(&secret, &envelope, &wire)?;
    account::unchanged(&record["secret"])?;
    let cache_session = cache.session_key().to_string();
    // 转换器标记上游真名，主程序 sse_model_rewrite 将它回写为客户端模型名。
    drive(opening, cache, &wire, &cache_session)
}
