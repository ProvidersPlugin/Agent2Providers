//! 会话/轮次的隔离因素与原生一致：账号/组织/调用方/首轮输入；工具续轮不改轮次。
//! WASM无7天全局表，使用账号私有随机盐+长度定界哈希派生不透明UUID，跨实例保持稳定。
//! 盐只在账号state；下游鉴权原文不进插件，只用宿主脱敏摘要参与隔离，不发送上游。

use super::{session::Session, util};
use agent2api_wasm_sdk::{
    json,
    state::{self, Scope},
    Error, Result, Value,
};
use sha2::{Digest, Sha256};

fn field(hash: &mut Sha256, name: &str, value: &[u8]) {
    hash.update((name.len() as u64).to_be_bytes());
    hash.update(name.as_bytes());
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value);
}
fn header(headers: &Value, name: &str) -> Option<String> {
    headers
        .as_object()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))?
        .1
        .as_str()
        .map(str::to_string)
}
fn id(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.len() <= 256 && value.bytes().all(|byte| byte.is_ascii_graphic()))
        .then(|| value.to_string())
}
fn codex(headers: &Value, key: &str) -> Option<String> {
    let raw = header(headers, "x-codex-turn-metadata").filter(|value| value.len() <= 16384)?;
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    parsed[key].as_str().and_then(id)
}
fn explicit_session(headers: &Value, body: &Value) -> Option<String> {
    for key in [
        "x-raccoon-session-id",
        "x-conversation-id",
        "ah-thread-id",
        "x-opencode-session",
        "x-session-id",
        "x-session-affinity",
        "session_id",
        "session-id",
    ] {
        if let Some(value) = header(headers, key).and_then(|value| id(&value)) {
            return Some(value);
        }
    }
    if let Some(value) = codex(headers, "session_id") {
        return Some(value);
    }
    for path in [
        "/session_id",
        "/conversation_id",
        "/metadata/session_id",
        "/metadata/conversation_id",
        "/prompt_cache_key",
    ] {
        if let Some(value) = body.pointer(path).and_then(Value::as_str).and_then(id) {
            return Some(value);
        }
    }
    let metadata = body
        .pointer("/metadata/user_id")
        .and_then(Value::as_str)?
        .trim();
    if metadata.starts_with('{') {
        let value: Value = serde_json::from_str(metadata).ok()?;
        return value["session_id"].as_str().and_then(id);
    }
    let (device, session) = metadata
        .strip_prefix("user_")?
        .split_once("_account__session_")?;
    (device.len() == 64 && device.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| id(session))
        .flatten()
}
fn explicit_turn(headers: &Value) -> Option<String> {
    for name in [
        "x-raccoon-turn-id",
        "x-conversation-request-id",
        "ah-trace-id",
        "x-turn-id",
    ] {
        if let Some(value) = header(headers, name).and_then(|value| id(&value)) {
            return Some(value);
        }
    }
    codex(headers, "turn_id")
}
fn salt() -> Result<String> {
    for _ in 0..3 {
        let state = state::get(Scope::Account, "conversation-salt", 1)?;
        if !state.compatible {
            return Err(Error::http(409, "小浣熊会话状态格式已变化"));
        }
        if let Some(value) = state.value.as_str().filter(|value| value.len() == 64) {
            return Ok(value.to_string());
        }
        let salt = util::random_hex(32)?;
        if state::compare_exchange(
            Scope::Account,
            "conversation-salt",
            1,
            state.revision,
            json!(salt),
            0,
        )? {
            return Ok(salt);
        }
    }
    Err(Error::http(409, "小浣熊会话标识正在更新"))
}
fn opaque(hash: Sha256) -> String {
    let mut bytes = hash.finalize().to_vec();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    util::uuid_of(&util::hex(&bytes))
}
pub fn headers(params: &Value, session: &Session) -> Result<Value> {
    let headers = &params["headers"];
    let body = &params["body"];
    let mut hash = Sha256::new();
    field(&mut hash, "salt", salt()?.as_bytes());
    field(&mut hash, "provider", b"raccoon");
    field(
        &mut hash,
        "account",
        session.record["id"].as_str().unwrap_or("").as_bytes(),
    );
    field(&mut hash, "org", session.credential.org.as_bytes());
    field(&mut hash, "identity", session.credential.user.as_bytes());
    if let Some(caller) = params["raccoonCaller"]
        .as_str()
        .filter(|caller| caller.len() == 64 && caller.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        // 原鉴权头由宿主移除，只消费原调用派生的脱敏关联摘要，不把它当授权。
        field(&mut hash, "caller", caller.as_bytes());
    }
    for name in ["x-client-id"] {
        if let Some(value) = header(headers, name) {
            field(&mut hash, name, value.as_bytes());
        }
    }
    if let Some(user) = body.get("user") {
        field(&mut hash, "user", user.to_string().as_bytes());
    }
    let messages = body["messages"].as_array();
    if let Some(id) = explicit_session(headers, body) {
        field(&mut hash, "session", id.as_bytes());
    } else {
        if let Some(agent) = header(headers, "user-agent") {
            field(&mut hash, "client", agent.as_bytes());
        }
        let first = messages
            .and_then(|rows| rows.iter().find(|row| row["role"] == "user"))
            .unwrap_or(body);
        field(&mut hash, "first-user", first.to_string().as_bytes());
    }
    let session_id = opaque(hash);
    let mut hash = Sha256::new();
    field(&mut hash, "session", session_id.as_bytes());
    if let Some(turn) = explicit_turn(headers) {
        field(&mut hash, "turn", turn.as_bytes());
    } else if let Some((rows, last)) = messages.and_then(|rows| {
        rows.iter()
            .rposition(|row| row["role"] == "user")
            .map(|last| (rows, last))
    }) {
        for row in &rows[..=last] {
            field(&mut hash, "message", row.to_string().as_bytes());
        }
    } else {
        field(&mut hash, "request", body.to_string().as_bytes());
    }
    Ok(json!({ "X-RACCOON-Session-ID": session_id, "X-RACCOON-Turn-ID": opaque(hash) }))
}
