//! 国际版对话指纹来自公开官方产物核对（plugins#367），不借用国内请求头。
//! Linux 对话省略 X-Tm 是 0.2.22 的已确认兼容方式，不冒充其它系统；业务头保留旧协议。
use super::{endpoints, util};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use md5::{Digest, Md5};

const APP_ID: &str = "100003";
const APP_KEY: &str = "38d2391985e2369a5fb8227d8e6cd5e5";
const OPENAI_VERSION: &str = "6.26.0";
const NODE_VERSION: &str = "v24.18.0";

fn platform() -> Result<&'static str> {
    match sdk::host("runtime.platform", json!({}))?.as_str() {
        Some("mac") => Ok("mac"),
        Some("win") => Ok("win"),
        Some("linux") => Ok("linux"),
        Some("other") => Ok("other"),
        _ => Err(Error::new("宿主未返回有效平台类型，不能生成国际版请求头")),
    }
}
fn signed_parts() -> Result<(String, String)> {
    let now = sdk::now();
    if now <= 0 {
        return Err(Error::new("宿主时钟不可用，不能生成 AutoClaw 请求签名"));
    }
    let timestamp = (now / 1000).to_string();
    let sign = format!(
        "{:x}",
        Md5::digest(format!("{APP_ID}&{timestamp}&{APP_KEY}").as_bytes())
    );
    Ok((timestamp, sign))
}
pub fn signed(token: &str) -> Result<Value> {
    let (timestamp, sign) = signed_parts()?;
    // 旧 userapi signed_auth_headers 与模型侧独立，不能只因模型头变化一并替换。
    let tm = if platform()? == "win" { "win" } else { "linux" };
    let mut headers = json!({ "Content-Type": "application/json", "Accept": "*/*",
        "X-Version": endpoints::CLIENT_VERSION, "X-Product": "autoclaw", "X-Client-Type": "pc",
        "X-Harness-Type": "zcode", "X-Tm": tm, "X-Lang": "zh-CN", "X-Channel": "official",
        "X-Auth-Appid": APP_ID, "X-Auth-TimeStamp": timestamp, "X-Auth-Sign": sign, "X-Trace-Id": util::uuid()? });
    let token = util::strip_bearer(token);
    if !token.is_empty() {
        util::token_valid(&token, false)?;
        headers["authorization"] = json!(format!("Bearer {token}"));
    }
    Ok(headers)
}
pub fn active(token: &str) -> Result<Value> {
    let token = util::strip_bearer(token);
    util::token_valid(&token, false)?;
    let (timestamp, sign) = signed_parts()?;
    // 当前活动明确按该独立业务渠道发放（plugins user-info / magpie#3681）；
    // 只供用户签到动作消费，不能混入登录、余额查询或模型发送以改变账号状态。
    Ok(json!({ "Content-Type": "application/json", "Accept": "*/*",
        "User-Agent": "AutoClaw/2.2.0 (win-x64)", "X-Version": "2.2.0", "X-Tm": "win-x64",
        "X-Product": "autoclaw", "X-Auth-Appid": APP_ID, "X-Auth-TimeStamp": timestamp,
        "X-Auth-Sign": sign, "X-Trace-Id": util::random_hex(16)?, "X-Lang": "zh-CN",
        "X-Channel": "AutoClaw20", "Authorization": format!("Bearer {token}") }))
}
pub fn chat(token: &str, route: &str, session: &str, input: &Value) -> Result<Value> {
    let token = util::strip_bearer(token);
    util::token_valid(&token, false)?;
    if !util::printable(route, 512) || route.is_empty() {
        return Err(Error::http(400, "AutoClaw 国际版模型路由名无效"));
    }
    if session.is_empty() || !util::printable(session, 2048) {
        return Err(Error::http(400, "AutoClaw 国际版会话关联无效"));
    }
    let (timestamp, sign) = signed_parts()?;
    let platform = platform()?;
    let os = match platform {
        "win" => "Windows",
        "mac" => "MacOS",
        "linux" => "Linux",
        _ => "Unknown",
    };
    let architecture = sdk::host("runtime.architecture", json!({}))?;
    let architecture = architecture
        .as_str()
        .ok_or_else(|| Error::new("宿主未返回有效架构类型"))?;
    let arch = match architecture {
        "x86_64" | "x64" => "x64",
        "aarch64" | "arm64" => "arm64",
        "x86" | "x32" => "x32",
        "arm" => "arm",
        _ => "unknown",
    };
    let request = util::uuid()?;
    let mut headers = json!({ "Content-Type": "application/json", "Accept": "application/json",
        "User-Agent": format!("OpenAI/JS {OPENAI_VERSION}"), "X-Stainless-Retry-Count": "0",
        "X-Stainless-Timeout": "600", "X-Stainless-Lang": "js", "X-Stainless-Package-Version": OPENAI_VERSION,
        "X-Stainless-OS": os, "X-Stainless-Arch": arch, "X-Stainless-Runtime": "node", "X-Stainless-Runtime-Version": NODE_VERSION,
        "X-Product": "autoclaw", "X-Client-Type": "pc", "X-Version": endpoints::CHAT_VERSION,
        "X-Lang": "en", "X-Channel": "official", "x_trace_id": "autoclaw-desktop",
        "X-Auth-Appid": APP_ID, "X-Auth-TimeStamp": timestamp, "X-Auth-Sign": sign,
        "X-Authorization": format!("Bearer {token}"), "X-Request-Id": request, "X-Trace-Id": request,
        "X-Request-Model": route, "X-Session-Id": session });
    if matches!(platform, "mac" | "win") {
        headers["X-Tm"] = json!(platform);
    }
    for (upstream, client) in [
        ("X-Session-Id", "x-autoclaw-session-id"),
        ("X-Agent-Id", "x-autoclaw-agent-id"),
        ("X-ZCode-Invocation-Id", "x-autoclaw-zcode-invocation-id"),
    ] {
        if let Some(value) = input
            .as_object()
            .into_iter()
            .flatten()
            .find(|(key, _)| key.eq_ignore_ascii_case(client))
            .and_then(|(_, value)| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if !util::printable(value, 2048) {
                return Err(Error::http(400, "AutoClaw 会话关联头格式无效"));
            }
            headers[upstream] = json!(value);
        }
    }
    Ok(headers)
}
