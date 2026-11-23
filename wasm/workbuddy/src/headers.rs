//! 官方国内请求身份分层，不把聊天头、匿名登录头和Cookie混用；随机只由宿主提供。
use super::{endpoints, util};
use agent2api_wasm_sdk::{json, Result, Value};

pub fn auth(secret: &Value) -> Value {
    let mut headers = json!({});
    let token = util::text(secret, &["accessToken"]);
    if !token.is_empty() {
        headers["Authorization"] = json!(format!("Bearer {token}"));
    }
    for (key, header) in [
        ("uid", "X-User-Id"),
        ("enterpriseId", "X-Enterprise-Id"),
        ("domain", "X-Domain"),
        ("departmentFullName", "X-Department-Info"),
    ] {
        let value = util::text(secret, &[key]);
        if !value.is_empty() {
            headers[header] = json!(value);
        }
    }
    if let Some(enterprise) = headers.get("X-Enterprise-Id").cloned() {
        headers["X-Tenant-Id"] = enterprise;
    }
    headers
}
pub fn framework(secret: &Value) -> Result<Value> {
    let mut headers = auth(secret);
    merge(
        &mut headers,
        json!({ "Accept": "application/json, text/plain, */*", "X-Requested-With": "XMLHttpRequest",
        "X-Request-ID": util::message_id()?, "X-Product": "SaaS", "User-Agent": endpoints::UA }),
    );
    if !util::text(secret, &["accessToken"]).is_empty() && headers["X-Domain"].is_null() {
        headers["X-Domain"] = json!("www.workbuddy.cn");
    }
    Ok(headers)
}
pub fn login(state: bool) -> Result<Value> {
    let mut headers = framework(&Value::Null)?;
    for key in [
        "X-No-Authorization",
        "X-No-User-Id",
        "X-No-Enterprise-Id",
        "X-No-Department-Info",
    ] {
        headers[key] = json!("true");
    }
    if state {
        headers["X-Domain"] = json!("www.workbuddy.cn");
        headers["X-Custom-Channel-Id"] = json!("unknown");
    }
    Ok(headers)
}
pub fn login_account(secret: &Value) -> Result<Value> {
    let mut headers = framework(secret)?;
    if headers["X-Domain"].is_null() {
        headers["X-Domain"] = json!("www.workbuddy.cn");
    }
    Ok(headers)
}
pub fn chat(secret: &Value) -> Result<Value> {
    let request = util::message_id()?;
    let message = util::message_id()?;
    let trace = util::message_id()?;
    let span = util::message_id()?[..16].to_string();
    let conversation = util::uuid_of(&util::message_id()?);
    let mut headers = auth(secret);
    merge(
        &mut headers,
        json!({ "Content-Type": "application/json", "Accept": "text/event-stream",
        "User-Agent": endpoints::UA, "X-IDE-Type": "WorkBuddy", "X-IDE-Name": "WorkBuddy", "X-IDE-Version": endpoints::VERSION,
        "X-Product": "SaaS", "X-Agent-Intent": "craft", "X-Agent-Purpose": "conversation", "X-Agent-Type": "main",
        "X-Agent-Runtime": "local-agent", "X-Private-Data": "true", "X-Requested-With": "XMLHttpRequest",
        "X-Conversation-ID": conversation, "X-Conversation-Request-ID": request, "X-Root-Request-ID": request,
        "X-Conversation-Message-ID": message, "X-Request-ID": message, "X-Trace-ID": trace,
        "traceparent": format!("00-{trace}-{span}-01"), "b3": format!("{trace}-{span}-1"),
        "X-B3-TraceId": trace, "X-B3-SpanId": span, "X-B3-Sampled": "1" }),
    );
    Ok(headers)
}
pub fn merge(target: &mut Value, fields: Value) {
    if let (Some(target), Some(fields)) = (target.as_object_mut(), fields.as_object()) {
        target.extend(fields.clone());
    }
}
