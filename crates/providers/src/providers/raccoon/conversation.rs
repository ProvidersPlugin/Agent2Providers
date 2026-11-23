//! 会话标识按调用方、账号和首轮输入隔离；工具续轮与重试复用同一轮次。

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use agent2api_provider_protocol::ChatRequest;
use serde_json::Value;
use sha2::{Digest, Sha256};

const IDLE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_SESSIONS: usize = 4096;
const MAX_TURNS: usize = 16384;

struct Entry {
    id: String,
    touched: Instant,
}

#[derive(Default)]
struct Ids {
    sessions: HashMap<String, Entry>,
    turns: HashMap<String, Entry>,
}

pub(super) fn headers(req: &ChatRequest, token: &str, org: &str) -> Vec<(String, String)> {
    let (session_key, turn_key) = keys(req, token, org);
    static IDS: OnceLock<Mutex<Ids>> = OnceLock::new();
    let mut ids = IDS
        .get_or_init(|| Mutex::new(Ids::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = Instant::now();
    let session = resolve(&mut ids.sessions, session_key, MAX_SESSIONS, now);
    let turn = resolve(&mut ids.turns, turn_key, MAX_TURNS, now);
    vec![
        ("X-RACCOON-Session-ID".to_string(), session),
        ("X-RACCOON-Turn-ID".to_string(), turn),
    ]
}

fn keys(req: &ChatRequest, token: &str, org: &str) -> (String, String) {
    let mut session = Sha256::new();
    field(&mut session, "provider", req.provider.as_bytes());
    field(&mut session, "account", req.account.as_bytes());
    field(&mut session, "org", org.as_bytes());
    let user = super::jwt::decode_jwt_claims(token)
        .map(|claims| super::jwt::extract_user_id(&claims))
        .filter(|user| !user.is_empty());
    field(
        &mut session,
        "identity",
        user.as_deref().unwrap_or(token).as_bytes(),
    );
    for name in ["authorization", "x-api-key", "x-client-id"] {
        if let Some(value) = req.headers.get(name) {
            field(&mut session, name, value.as_bytes());
        }
    }
    if let Some(user) = req.body.get("user") {
        field(&mut session, "user", user.to_string().as_bytes());
    }
    let messages = req.body.get("messages").and_then(Value::as_array);
    if let Some(explicit) = session_id(&req.headers, &req.body) {
        field(&mut session, "session", explicit.as_bytes());
    } else {
        if let Some(agent) = req.headers.get("user-agent") {
            field(&mut session, "client", agent.as_bytes());
        }
        if let Some(first) =
            messages.and_then(|messages| messages.iter().find(|message| is_user(message)))
        {
            field(&mut session, "first-user", first.to_string().as_bytes());
        } else {
            field(&mut session, "request", req.body.to_string().as_bytes());
        }
    }
    let session = format!("{:x}", session.finalize());
    let mut turn = Sha256::new();
    field(&mut turn, "session", session.as_bytes());
    if let Some(explicit) = turn_id(&req.headers) {
        field(&mut turn, "turn", explicit.as_bytes());
    } else if let Some((messages, end)) = messages.and_then(|messages| {
        messages
            .iter()
            .rposition(is_user)
            .map(|end| (messages, end))
    }) {
        // 后续工具结果位于最后一条用户消息之后，不能改变这一轮的标识。
        for message in &messages[..=end] {
            field(&mut turn, "message", message.to_string().as_bytes());
        }
    } else {
        field(&mut turn, "request", req.body.to_string().as_bytes());
    }
    (session, format!("{:x}", turn.finalize()))
}

fn field(hash: &mut Sha256, name: &str, value: &[u8]) {
    hash.update((name.len() as u64).to_be_bytes());
    hash.update(name.as_bytes());
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value);
}

fn session_id(headers: &BTreeMap<String, String>, body: &Value) -> Option<String> {
    for name in [
        "x-raccoon-session-id",
        "x-conversation-id",
        "ah-thread-id",
        "x-opencode-session",
        "x-session-id",
        "x-session-affinity",
        "session_id",
        "session-id",
    ] {
        if let Some(value) = headers.get(name).and_then(|value| identifier(value)) {
            return Some(value);
        }
    }
    if let Some(value) = codex_metadata(headers, "session_id") {
        return Some(value);
    }
    for pointer in [
        "/session_id",
        "/conversation_id",
        "/metadata/session_id",
        "/metadata/conversation_id",
    ] {
        if let Some(value) = body
            .pointer(pointer)
            .and_then(Value::as_str)
            .and_then(identifier)
        {
            return Some(value);
        }
    }
    if let Some(value) = body
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .and_then(identifier)
    {
        return Some(value);
    }
    let metadata = body
        .pointer("/metadata/user_id")
        .and_then(Value::as_str)?
        .trim();
    if metadata.starts_with('{') {
        let data: Value = serde_json::from_str(metadata).ok()?;
        return data
            .get("session_id")
            .and_then(Value::as_str)
            .and_then(identifier);
    }
    let (device, session) = metadata
        .strip_prefix("user_")?
        .split_once("_account__session_")?;
    (device.len() == 64 && device.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| identifier(session))
        .flatten()
}

fn turn_id(headers: &BTreeMap<String, String>) -> Option<String> {
    for name in [
        "x-raccoon-turn-id",
        "x-conversation-request-id",
        "ah-trace-id",
        "x-turn-id",
    ] {
        if let Some(value) = headers.get(name).and_then(|value| identifier(value)) {
            return Some(value);
        }
    }
    codex_metadata(headers, "turn_id")
}

fn codex_metadata(headers: &BTreeMap<String, String>, name: &str) -> Option<String> {
    let raw = headers
        .get("x-codex-turn-metadata")
        .filter(|value| value.len() <= 16384)?;
    let value: Value = serde_json::from_str(raw).ok()?;
    value.get(name).and_then(Value::as_str).and_then(identifier)
}

fn identifier(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.len() <= 256 && value.bytes().all(|byte| byte.is_ascii_graphic()))
        .then(|| value.to_string())
}

fn is_user(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("user")
}

fn resolve(entries: &mut HashMap<String, Entry>, key: String, cap: usize, now: Instant) -> String {
    entries.retain(|_, entry| now.saturating_duration_since(entry.touched) < IDLE_TTL);
    if let Some(entry) = entries.get_mut(&key) {
        entry.touched = now;
        return entry.id.clone();
    }
    if entries.len() >= cap {
        if let Some(oldest) = entries
            .iter()
            .min_by_key(|(_, entry)| entry.touched)
            .map(|(key, _)| key.clone())
        {
            entries.remove(&oldest);
        }
    }
    let id = crate::ids::uuid_v4();
    entries.insert(
        key,
        Entry {
            id: id.clone(),
            touched: now,
        },
    );
    id
}
