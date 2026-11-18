use crate::{host, json, Error, Result, Value};
use crate::state::{self, Scope};

const PREFIX: &str = "cookie_login_";

pub fn start(provider: &str, origin: &str, hosts: &[&str]) -> Result<Value> {
    use base64::Engine;
    let raw = host("random.bytes", json!({"length": 24}))?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(raw.as_str().unwrap_or(""))
        .map_err(|_| Error::new("登录随机值无效"))?;
    if bytes.len() != 24 { return Err(Error::new("登录随机值无效")); }
    let flow = format!("{PREFIX}{}", bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>());
    host("session.put", json!({"id":flow,"ttl_ms":300_000,"value":{"provider":provider,"state":"waiting"}}))?;
    let url = if provider.starts_with("monkeycode") { format!("{origin}/login") } else { format!("{origin}/") };
    Ok(json!({"flow":flow,"url":url,"poll_interval_ms":0,"external_browser":false,"allowed_hosts":hosts,
        "instructions":"请在桌面内嵌窗口完成官方登录，成功后自动添加账号；网页面板可使用手工粘贴凭证。"}))
}

pub fn callback(provider: &str, params: &Value, normalize: impl FnOnce(&Value) -> Result<Value>) -> Result<Value> {
    let flow = params["flow"].as_str().filter(|flow| flow.starts_with(PREFIX) && flow.len() <= 128)
        .ok_or_else(|| Error::http(400,"登录流程无效"))?;
    let pending = host("session.get", json!({"id":flow}))?;
    if pending["provider"] != provider || pending["state"] != "waiting" {
        return Err(Error::http(409,"登录流程已经取消、过期或属于其他提供商"));
    }
    let claimed = state::get(Scope::Login, "cookie.consume", 1)?;
    if !claimed.value.is_null() || !state::compare_exchange(Scope::Login, "cookie.consume", 1, claimed.revision, json!({"processing":true}), 300_000)? {
        return Err(Error::http(409,"本次登录回调正在处理或已被使用"));
    }
    let code = params["code"].as_str().filter(|code| !code.is_empty() && code.len() <= 256 * 1024)
        .ok_or_else(|| Error::http(400,"登录窗口没有提供有效凭证"))?;
    let payload = serde_json::from_str::<Value>(code).unwrap_or_else(|_| json!({"cookie":code}));
    let normalized = normalize(&json!({"provider":provider,"source":{"kind":"manual","payload":payload}}));
    let current = host("session.get", json!({"id":flow}))?;
    if current["provider"] != provider || current["state"] != "waiting" { return Err(Error::http(409,"登录已取消，不保存账号")); }
    host("session.remove", json!({"id":flow}))?;
    normalized.map(|account| json!({"state":"done","account":account}))
}

pub fn cancel(params: &Value) -> Result<Value> {
    if let Some(flow) = params["flow"].as_str().filter(|flow| flow.starts_with(PREFIX)) {
        host("session.remove", json!({"id":flow}))?;
    }
    Ok(Value::Null)
}
