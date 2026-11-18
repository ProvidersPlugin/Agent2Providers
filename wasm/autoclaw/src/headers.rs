//! 国内 userapi 固定客户端签名。公开客户端 appKey 不是用户密钥。
//! 对话和 userapi 的头集合刻意不同：聊天不发 X-Harness-Type，避免已确认的 403/406。
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use md5::{Digest, Md5};
use super::{endpoints, util};

const APP_ID: &str = "100003";
const APP_KEY: &str = "38d2391985e2369a5fb8227d8e6cd5e5";

pub fn signed(token: &str) -> Result<Value> {
    let now = sdk::now();
    if now <= 0 { return Err(Error::new("宿主时钟不可用，不能生成 AutoClaw 请求签名")); }
    let timestamp = (now / 1000).to_string();
    let sign = format!("{:x}", Md5::digest(format!("{APP_ID}&{timestamp}&{APP_KEY}").as_bytes()));
    let mut headers = json!({ "Content-Type": "application/json", "Accept": "*/*",
        "X-Version": endpoints::CLIENT_VERSION, "X-Product": "autoclaw", "X-Client-Type": "pc",
        "X-Harness-Type": "zcode", "X-Tm": "win", "X-Lang": "zh-CN", "X-Channel": "official",
        "X-Auth-Appid": APP_ID, "X-Auth-TimeStamp": timestamp, "X-Auth-Sign": sign, "X-Trace-Id": util::uuid()? });
    let token = util::strip_bearer(token);
    if !token.is_empty() { util::token_valid(&token, false)?; headers["authorization"] = json!(format!("Bearer {token}")); }
    Ok(headers)
}

pub fn chat(token: &str, route: &str, input: &Value) -> Result<Value> {
    let token = util::strip_bearer(token);
    util::token_valid(&token, false)?;
    if !util::printable(route, 512) || route.is_empty() { return Err(Error::http(400, "AutoClaw 模型路由名无效")); }
    let mut headers = json!({ "Content-Type": "application/json", "Accept": "*/*",
        "X-Product": "autoclaw", "X-Client-Type": "pc", "X-Tm": "win", "X-Version": endpoints::CHAT_VERSION,
        "X-Lang": "zh-CN", "X-Channel": "official", "x_trace_id": "autoclaw-desktop",
        "X-Authorization": format!("Bearer {token}"), "X-Request-Id": util::uuid()?, "X-Request-Model": route });
    for (upstream, client) in [("X-Session-Id", "x-autoclaw-session-id"), ("X-Agent-Id", "x-autoclaw-agent-id"),
        ("X-ZCode-Invocation-Id", "x-autoclaw-zcode-invocation-id")] {
        if let Some(value) = input.as_object().into_iter().flatten().find(|(key, _)| key.eq_ignore_ascii_case(client))
            .and_then(|(_, value)| value.as_str()).map(str::trim).filter(|value| !value.is_empty()) {
            if !util::printable(value, 2048) { return Err(Error::http(400, "AutoClaw 会话关联头格式无效")); }
            headers[upstream] = json!(value);
        }
    }
    Ok(headers)
}
