use agent2api_wasm_sdk::{json, Result, Value};
use super::network;

const SESSION_HEADERS: [&str; 7] = ["x-opencode-session", "x-claude-code-session-id", "claude-code-session-id",
    "x-vscode-sessionid", "x-session-id", "x-parent-session-id", "helicone-session-id"];

pub struct Context { pub headers: Value, pub metadata_user: String }

fn printable(value: &Value) -> Option<String> {
    let value = value.as_str()?.trim();
    (!value.is_empty() && value.len() <= 2048 && value.bytes().all(|byte| (32..=126).contains(&byte))).then(|| value.to_string())
}

fn pick(value: &Value, keys: &[&str]) -> Option<String> { keys.iter().find_map(|key| printable(&value[*key])) }
fn strip(mut value: &str, prefixes: &[&str]) -> String {
    for prefix in prefixes { if let Some(rest) = value.strip_prefix(*prefix).filter(|rest| !rest.is_empty()) { value = rest; } }
    value.to_string()
}

impl Context {
    pub fn new(headers: &Value, body: &Value, mid: &str) -> Result<Self> {
        let headers = headers.as_object().map(|fields| Value::Object(fields.iter()
            .map(|(key, value)| (key.to_ascii_lowercase(), value.clone())).collect())).unwrap_or_else(|| json!({}));
        let metadata = &body["metadata"];
        let user: Value = serde_json::from_str(metadata["user_id"].as_str().unwrap_or("")).unwrap_or(Value::Null);
        let session = strip(&pick(&headers, &SESSION_HEADERS)
            .or_else(|| pick(metadata, &["sessionId", "session_id", "conversationId", "conversation_id"]))
            .or_else(|| pick(&user, &["session_id"])).unwrap_or(network::uuid()?), &["sess_", "subagent_agent_"]);
        let request = pick(&headers, &["x-request-id", "x-client-request-id"])
            .or_else(|| pick(metadata, &["requestId", "request_id"])).unwrap_or(network::uuid()?);
        let trace = pick(&headers, &["x-zcode-trace-id"])
            .or_else(|| pick(metadata, &["traceId", "trace_id"])).unwrap_or(network::uuid()?);
        let query = pick(&headers, &["x-query-id"]).or_else(|| pick(metadata, &["queryId", "query_id"]));
        let mut headers = json!({ "x-request-id": request, "x-zcode-session-type": "main",
            "x-zcode-trace-id": trace, "x-session-id": session, "x-zcode-query-source": "main_turn" });
        if let Some(query) = query { headers["x-query-id"] = json!(strip(&query, &["query_"])); }
        let mut user = json!({ "account_uuid": "", "session_id": session });
        if !mid.is_empty() { user["device_id"] = json!(mid); }
        Ok(Self { headers, metadata_user: user.to_string() })
    }
}
