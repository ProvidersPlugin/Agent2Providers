//! 国际请求身份按原生Region::Intl分组：后台UA与对话UA不同，网页请求不得混入桌面头。
use crate::{account, endpoints, util};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    Error, Result, Value,
};

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
    if let Some(value) = headers.get("X-Enterprise-Id").cloned() {
        headers["X-Tenant-Id"] = value;
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
    Ok(headers)
}
pub fn refresh(secret: &Value) -> Result<Value> {
    let mut headers = framework(secret)?;
    // 原生auth与refresh_identity传daemon=false，使用国际chat_ua，而目录/计费使用daemon=true。
    headers["User-Agent"] = json!(endpoints::CHAT_UA);
    headers["X-Auth-Refresh-Source"] = json!("plugin");
    if headers["X-Domain"].is_null() {
        headers["X-Domain"] = json!("www.workbuddy.ai");
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
        headers["X-Domain"] = json!("www.workbuddy.ai");
        headers["X-Custom-Channel-Id"] = json!("unknown");
    }
    Ok(headers)
}
pub fn login_account(secret: &Value, initial: bool) -> Result<Value> {
    let mut headers = framework(secret)?;
    if headers["X-Domain"].is_null() {
        headers["X-Domain"] = json!("www.workbuddy.ai");
    }
    if initial {
        for key in ["X-No-User-Id", "X-No-Enterprise-Id", "X-No-Department-Info"] {
            headers[key] = json!("true");
        }
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
        "User-Agent": endpoints::CHAT_UA, "X-IDE-Type": "WorkBuddy", "X-IDE-Name": "WorkBuddy", "X-IDE-Version": endpoints::VERSION,
        "X-Product": "SaaS", "X-Agent-Intent": "craft", "X-Agent-Purpose": "conversation", "X-Agent-Type": "main",
        "X-Private-Data": "true", "X-Requested-With": "XMLHttpRequest", "X-Domain": "www.workbuddy.ai",
        "X-Conversation-ID": conversation, "X-Conversation-Request-ID": request, "X-Root-Request-ID": request,
        "X-Conversation-Message-ID": message, "X-Request-ID": message, "X-Trace-ID": trace,
        "traceparent": format!("00-{trace}-{span}-01"), "b3": format!("{trace}-{span}-1"),
        "X-B3-TraceId": trace, "X-B3-SpanId": span, "X-B3-Sampled": "1" }),
    );
    // 国际版不带国内专属X-Agent-Runtime，不根据旧注释添加额外Origin/Referer。
    Ok(headers)
}
/// 国际上游按轮次头计费；工具续接保留同一用户发送标识，不把每一步当新对话。
/// caller 只由宿主原真实对话派生，缺失时保持单次随机标识，不能凭会话字符串跨调用方合并。
pub fn conversation(secret: &Value, params: &Value) -> Result<Value> {
    let mut headers = chat(secret)?;
    let caller = params["workbuddyIntlCaller"]
        .as_str()
        .filter(|caller| caller.len() == 64 && caller.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let session = params["session"]
        .as_str()
        .filter(|session| !session.is_empty());
    let messages = params["body"]["messages"].as_array();
    let first =
        messages.and_then(|messages| messages.iter().find(|message| message["role"] == "user"));
    if let Some(caller) = caller.filter(|_| session.is_some() || first.is_some()) {
        let key = util::hash(
            &json!({
                "caller": caller, "authority": account::authority(secret),
                "session": session, "first": if session.is_none() { first } else { None },
            })
            .to_string(),
        );
        let salt = tracking_salt(&key)?;
        let conversation = util::uuid_of(&util::hash(&format!("intl-conversation:{salt}"))[..32]);
        // 本轮最后的用户消息之后是助手/工具续接，续接时不得改掉原用户轮次标识。
        let history = messages.map(|messages| {
            let end = messages
                .iter()
                .rposition(|message| message["role"] == "user");
            messages[..end.map(|end| end + 1).unwrap_or(messages.len())].to_vec()
        });
        let turn = &util::hash(&json!({ "salt": salt, "history": history }).to_string())[..32];
        headers["X-Conversation-ID"] = json!(conversation);
        headers["X-Conversation-Request-ID"] = json!(turn);
        headers["X-Root-Request-ID"] = json!(turn);
    }
    Ok(headers)
}
fn tracking_salt(key: &str) -> Result<String> {
    const KEY: &str = "intl-chat-tracking";
    for _ in 0..3 {
        let before = state::get(Scope::Account, KEY, 1)?;
        if !before.compatible {
            return Err(Error::http(409, "国际会话追踪状态版本已变化"));
        }
        let now = sdk::now();
        let mut entries = before.value["entries"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        entries.retain(|entry| {
            entry["at"]
                .as_i64()
                .is_some_and(|at| at <= now && now.saturating_sub(at) < 7 * 86_400_000)
        });
        let salt = if let Some(entry) = entries.iter_mut().find(|entry| entry["key"] == key) {
            let salt = entry["salt"]
                .as_str()
                .filter(|salt| {
                    salt.len() == 32 && salt.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
                .ok_or_else(|| Error::http(409, "国际会话追踪数据无效"))?
                .to_string();
            entry["at"] = json!(now);
            salt
        } else {
            let salt = util::random_hex(16)?;
            while entries.len() >= 256 {
                entries.remove(0);
            }
            entries.push(json!({ "key": key, "salt": salt, "at": now }));
            salt
        };
        // 只保存摘要、随机盐和时间；不保存鉴权头、会话原文或工具结果。
        if state::compare_exchange(
            Scope::Account,
            KEY,
            1,
            before.revision,
            json!({ "entries": entries }),
            0,
        )? {
            return Ok(salt);
        }
    }
    Err(Error::http(409, "国际会话追踪正在更新，未发送请求"))
}
pub fn merge(target: &mut Value, fields: Value) {
    if let (Some(target), Some(fields)) = (target.as_object_mut(), fields.as_object()) {
        target.extend(fields.clone());
    }
}
