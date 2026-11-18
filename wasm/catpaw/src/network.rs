//! 仅两个封闭的业务调用，不向调用方开放任意 URL/path/header 接口。
//! 目录用 Cookie + user-uid；积分用同一 accessToken 的 X-Auth-Token。出口/UA由宿主绑定。
//! http.timeout.v1：目录 30 秒、积分 15 秒；从宿主收到 open 起覆盖完整响应体，与可信外层剩余预算取小。
//! 超时结束本次请求，不自动重试、切号或回落直连；旧宿主不支持该能力时拒绝加载。

use agent2api_wasm_sdk::{http, json, Result, Value};

use crate::{credentials::Credentials, endpoints, errors, util};

pub fn catalog(credentials: &Credentials) -> Result<Value> {
    credentials.require_main()?;
    let headers = json!({ "Accept": "application/json", "Content-Type": "application/json",
        "Cookie": format!("X-Passport-Token={}", credentials.token), "user-uid": credentials.user(),
        "M-APPKEY": endpoints::APP_KEY, "M-TRACEID": util::trace_id()?,
        "gray-set": "new-agent-sdk", "enableHeartBeat": "true", "X-Agent-Version": endpoints::CLIENT_VERSION });
    let body = json!({ "tenant": "CatDesk", "scene": "CATX_APP", "env": "EXTERNAL" });
    let response = http::open_timeout("POST", endpoints::MODELS_URL, headers, Some(&body), 30_000u64)
        .map_err(errors::transport)?;
    let value = read(response, "模型目录查询")?;
    match value["code"].as_i64() {
        Some(0 | 200) => value.get("data").filter(|value| value.is_array()).cloned()
            .ok_or_else(|| errors::protocol("CatPaw 模型目录响应缺少有效 data 数组")),
        Some(code) => Err(errors::upstream(business_status(&value), Some(code), "模型目录查询")),
        None => Err(errors::protocol("CatPaw 模型目录响应缺少明确的数字成功码")),
    }
}

pub fn balance(credentials: &Credentials) -> Result<Value> {
    let token = credentials.balance_credential()?;
    let headers = json!({ "Accept": "application/json", "X-Auth-Token": token, "gray-set": "new-agent-sdk" });
    let response = http::open_timeout("GET", endpoints::BALANCE_URL, headers, None, 15_000u64)
        .map_err(errors::transport)?;
    let value = read(response, "积分查询")?;
    let code = value["code"].as_i64();
    if matches!(code, Some(4010 | 4011)) { return Err(errors::upstream(401, code, "积分查询")); }
    match code {
        Some(0) => value.get("data").filter(|value| value.is_object()).cloned()
            .ok_or_else(|| errors::protocol("CatPaw 积分响应缺少有效 data 对象")),
        Some(code) => Err(errors::upstream(business_status(&value), Some(code), "积分查询")),
        None => Err(errors::protocol("CatPaw 积分响应缺少明确的数字成功码")),
    }
}

fn read(response: http::Response, operation: &str) -> Result<Value> {
    let status = response.status;
    if !(200..300).contains(&status) {
        // 错误响应不是业务资料：不读出 HTML/令牌正文再放进公开错误。
        return Err(errors::upstream(status, None, operation));
    }
    let bytes = response.bytes(2 * 1024 * 1024).map_err(errors::transport)?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| errors::protocol("CatPaw 上游未返回有效 JSON"))?;
    if !value.is_object() { return Err(errors::protocol("CatPaw 上游响应必须是 JSON 对象")); }
    if matches!(value["code"].as_i64(), Some(0 | 200))
        && (value.get("error").is_some_and(|value| !value.is_null()) || value["success"] == false
            || value["type"] == "error") {
        return Err(errors::upstream(business_status(&value), value["code"].as_i64(), operation));
    }
    Ok(value)
}

fn business_status(value: &Value) -> u16 {
    value["httpStatus"].as_u64().and_then(|status| u16::try_from(status).ok())
        .filter(|status| (400..500).contains(status)).unwrap_or(502)
}
