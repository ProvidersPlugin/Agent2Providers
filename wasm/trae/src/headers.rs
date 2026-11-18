//! 原生聊天三令牌头与UG独立画像。设备值只来自凭据，OS族来自runtime.platform。
//! 不冒充Windows 11 Pro/83DG/真实电脑名；完整OS与硬件型号没有可用主机事实，省略。

use super::{credentials, endpoints, util};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub fn platform() -> Result<&'static str> {
    match sdk::host("runtime.platform", json!({}))?.as_str() {
        Some("win") => Ok("windows"),
        Some("mac") => Ok("mac"),
        Some("linux") => Ok("linux"),
        Some("other") => Ok("other"),
        _ => Err(Error::http(503, "主程序未提供有效平台类型")),
    }
}

pub fn oauth(token: Option<&str>) -> Value {
    let mut value = json!({ "accept": "application/json", "content-type": "application/json",
        "user-agent": format!("Trae/{}", endpoints::IDE_VERSION) });
    if let Some(token) = token {
        value["x-cloudide-token"] = json!(token);
    }
    value
}

pub fn chat(secret: &Value, stream: bool) -> Result<Value> {
    let token = credentials::token(secret)?;
    let request = util::uuid()?;
    let mut h = json!({ "accept": if stream { "text/event-stream" } else { "application/json" },
        "content-type": "application/json", "user-agent": format!("Trae/{}", endpoints::IDE_VERSION),
        "authorization": format!("Cloud-IDE-JWT {token}"), "x-cloudide-token": token, "x-ide-token": token,
        "x-app-id": endpoints::APP_ID, "x-app-version": endpoints::IDE_VERSION, "x-ide-version": endpoints::IDE_VERSION,
        "x-ide-version-code": endpoints::VERSION_CODE, "x-app-version-code": endpoints::VERSION_CODE,
        "x-ide-version-type": "stable", "x-device-type": platform()?,
        "x-request-id": request, "x-trae-request-id": request,
        "x-plugin-channel": "icube-ai", "request-traffic-type": "prod" });
    for (field, header) in [
        ("uid", "x-uid"),
        ("machineId", "x-machine-id"),
        ("deviceId", "x-device-id"),
    ] {
        let value = util::header(&util::text(secret, &[field]), 480)?;
        if !value.is_empty() {
            h[header] = json!(value);
        }
    }
    Ok(h)
}

pub fn ug(secret: &Value, device: &str, bearer: bool) -> Result<Value> {
    let token = credentials::token(secret)?;
    let request = util::uuid()?;
    let compact = request.replace('-', "");
    let trace = format!("00-{}-{}-01", util::random_hex(16)?, &compact[..16]);
    let mut h = json!({ "content-type": "application/json", "accept": "*/*",
        "user-agent": if credentials::variant(secret) == "cn" { "VSCode 1.107.1 (TRAE CN)" } else { "VSCode 1.107.1 (TRAE SOLO CN)" },
        "x-user-region": "CN", "accept-language": "zh-CN", "package-type": "stable_cn",
        "x-lgw-req-sdk-type": "3", "x-market-client-id": "VSCode 1.107.1", "x-device-type": platform()?,
        "x-app-version": endpoints::IDE_VERSION, "app-version": endpoints::IDE_VERSION,
        "x-request-id": request, "x-tt-trace-id": trace,
        "sec-fetch-dest": "empty", "sec-fetch-mode": "no-cors", "sec-fetch-site": "none",
        "authorization": format!("{} {token}", if bearer { "Bearer" } else { "Cloud-IDE-JWT" }) });
    if !device.is_empty() {
        h["x-device-id"] = json!(util::header(device, 256)?);
    }
    Ok(h)
}
