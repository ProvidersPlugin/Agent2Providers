//! 仅用当前可信managed Call：宿主回调租约、原流程有界状态、一次输入与身份归一化。
//! 三条固定网络请求均由宿主持原出口执行；插件不取任意token、不开监听、不自行轮询。
use agent2api_wasm_sdk::{self as sdk, json, login::managed, state::{self, Scope}, Error, Result, Value};

use crate::{account, endpoints, errors, login_methods, util};

const KEY: &str = "passport-login";
const PROFILE: &str = "catpaw.passport.v1";
const ORIGIN: &str = "https://catpaw.meituan.com";
const ENTRY: &str = "https://catpaw.meituan.com/api/gateway/passport/login-entry";
const TTL: i64 = 300_000;

fn invalid() -> Error { Error::http(409, "CatPaw 托管登录已结束、过期或步骤已变化，请重新发起").with_code("catpaw_login_expired") }

fn flow(params: &Value) -> Result<&str> {
    params["flow"].as_str().filter(|flow| flow.starts_with("managed_") && flow.len() <= 256
        && !flow.chars().any(|ch| ch.is_whitespace() || ch.is_control())).ok_or_else(invalid)
}

fn context(params: &Value) -> Result<managed::Context> {
    let context = managed::context().map_err(errors::host)?;
    if context.provider != endpoints::PROVIDER || context.method_index != 0 || context.flow != flow(params)?
        || context.interaction.is_empty() || context.remaining_ms == 0 || context.expires_at <= util::now()?
        || context.authorization_origins.len() != 1 || context.authorization_origins[0] != ORIGIN { return Err(invalid()); }
    Ok(context)
}

fn current(context: &managed::Context) -> Result<state::Snapshot> {
    let snapshot = state::get(Scope::Login, KEY, 1)?;
    if !snapshot.compatible || snapshot.value["flow"] != context.flow
        || snapshot.value["expiresAt"].as_i64() != Some(context.expires_at)
        || snapshot.value["interaction"].as_str() != Some(context.interaction.as_str()) { return Err(invalid()); }
    Ok(snapshot)
}

fn save(before: &state::Snapshot, value: Value, expires_at: i64) -> Result<state::Snapshot> {
    let ttl = expires_at.saturating_sub(util::now()?).min(TTL);
    if ttl <= 0 || !state::compare_exchange(Scope::Login, KEY, 1, before.revision, value.clone(), ttl)? { return Err(invalid()); }
    let after = state::get(Scope::Login, KEY, 1)?;
    if !after.compatible || after.value != value { return Err(invalid()); }
    Ok(after)
}

fn clear(before: &state::Snapshot) -> Result<()> {
    if !state::compare_exchange(Scope::Login, KEY, 1, before.revision, Value::Null, 1000)? { return Err(invalid()); }
    Ok(())
}

fn exact(value: &Value, fields: &[&str]) -> Result<()> {
    if value.as_object().is_none_or(|value| value.len() != fields.len()
        || value.keys().any(|key| !fields.contains(&key.as_str()))) { return Err(invalid()); }
    Ok(())
}

fn host_text<'a>(value: &'a Value, key: &str, max: usize) -> Result<&'a str> {
    value[key].as_str().filter(|value| !value.is_empty() && value.len() <= max
        && !value.chars().any(|ch| ch.is_control() || ch.is_whitespace())).ok_or_else(invalid)
}

fn hex32(value: &str) -> bool { value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')) }

fn encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) { encoded.push(byte as char); }
        else { encoded.push_str(&format!("%{byte:02X}")); }
    }
    encoded
}

pub fn start(params: &Value) -> Result<Value> {
    let context = context(params)?;
    if params["payload"]["methodIndex"].as_u64().is_some_and(|index| index != 0) { return Err(invalid()); }
    let before = state::get(Scope::Login, KEY, 1)?;
    if !before.value.is_null() { return Err(invalid()); }
    let reserved = save(&before, json!({ "flow": context.flow, "interaction": context.interaction,
        "expiresAt": context.expires_at, "phase": "preparing" }), context.expires_at)?;
    // typed SDK回执未声明本profile的附加字段；只读现有host结果，不修改共享SDK。
    let lease = sdk::host("login.loopback.open", json!({ "profile": PROFILE, "expected": context.interaction })).map_err(errors::host)?;
    exact(&lease, &["leaseId", "callbackUrl", "expiresAt", "state", "sid", "authorizationUrl"])?;
    let id = host_text(&lease, "leaseId", 128)?;
    let callback = host_text(&lease, "callbackUrl", 128)?;
    let state = host_text(&lease, "state", 32)?;
    let sid = host_text(&lease, "sid", 32)?;
    let url = host_text(&lease, "authorizationUrl", 2048)?;
    if !hex32(state) || !hex32(sid) || state == sid || lease["expiresAt"].as_i64() != Some(context.expires_at) { return Err(invalid()); }
    let port = callback.strip_prefix("http://127.0.0.1:").and_then(|part| part.strip_suffix("/catpaw/callback"))
        .filter(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())).ok_or_else(invalid)?;
    let number = port.parse::<u16>().ok().filter(|port| *port != 0).ok_or_else(invalid)?;
    if number.to_string() != port || url != format!("{ENTRY}?state={state}&redirect={}&sid={sid}", encode(callback)) { return Err(invalid()); }
    let text = login_methods::web_text(params["payload"]["locale"].as_str().unwrap_or("zh-CN"));
    let step = sdk::host("login.step.publish", json!({ "expected": context.interaction,
        "title": text[0], "message": text[1], "captcha": null,
        "browser": { "url": url, "leaseId": id, "browserMode": "external", "manualCallback": false, "callbackInput": "url" } }))?;
    let interaction = host_text(&step, "id", 128)?;
    if step["provider"] != context.provider || step["flow"] != context.flow || step["methodIndex"] != 0
        || step["status"] != "ready" || step["browser"]["manualCallback"] != false || interaction == context.interaction { return Err(invalid()); }
    save(&reserved, json!({ "flow": context.flow, "interaction": interaction, "expiresAt": context.expires_at,
        "phase": "waiting", "leaseId": id, "state": state }), context.expires_at)?;
    Ok(json!({ "flow": context.flow, "url": "", "poll_interval_ms": 1000,
        "instructions": text[1], "external_browser": true, "allowed_hosts": ["catpaw.meituan.com"] }))
}

pub fn callback(params: &Value) -> Result<Value> {
    let context = context(params)?;
    let before = current(&context)?;
    if before.value["phase"] != "waiting" { return Err(invalid()); }
    let input = managed::take_input().map_err(errors::host)?;
    exact(&input, &["token", "state", "channel", "user"])?;
    if input["state"] != before.value["state"] || !matches!(input["channel"].as_str(), Some("loopback" | "poll")) { return Err(invalid()); }
    exact(&input["user"], &["uid", "loginName"])?;
    // 进入单次归一化前清掉flow状态；异常不缓存token、不恢复waiting、不再次取件。
    clear(&before)?;
    let result = account::from_browser(&input)?;
    context_after(params, &context)?;
    Ok(sdk::login::done(result))
}

fn context_after(params: &Value, before: &managed::Context) -> Result<()> {
    let after = context(params)?;
    if before.flow != after.flow || before.interaction != after.interaction || before.expires_at != after.expires_at { return Err(invalid()); }
    Ok(())
}

pub fn poll(params: &Value) -> Result<Value> {
    let context = context(params)?;
    let before = current(&context)?;
    if before.value["phase"] != "waiting" { return Err(invalid()); }
    // 固定poll-token由宿主的单一调度器执行；不让普通RPC或UI恢复触发第二条取件链。
    Ok(sdk::login::pending(None))
}

pub fn cancel(params: &Value) -> Result<Value> {
    let flow = flow(params)?;
    let before = state::get(Scope::Login, KEY, 1)?;
    if !before.value.is_null() && before.value["flow"] != flow { return Err(invalid()); }
    clear(&before)?;
    Ok(Value::Null)
}
