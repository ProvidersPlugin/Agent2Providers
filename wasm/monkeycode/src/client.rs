//! 所有实际出站都从五个只读端点枚举构造。没有通用 URL／method／任务创建入口。
//! Cookie 只发送目标会话，不透传其他 Cookie；代理由当前宿主账号上下文决定。

use agent2api_wasm_sdk::{http, json, Result, Value};

use crate::{
    endpoints::{self, ReadEndpoint},
    errors, util,
};

pub fn get(session: &str, endpoint: ReadEndpoint, query: &[(&str, String)]) -> Result<Value> {
    let response = http::open(
        "GET",
        &endpoint.url(query)?,
        json!({
            "Cookie": format!("{}={session}", endpoints::SESSION_COOKIE),
            "Origin": endpoints::ORIGIN,
            "Referer": format!("{}/console/", endpoints::ORIGIN),
            "Accept": "application/json"
        }),
        None,
    )
    .map_err(|error| errors::transport(error, endpoint))?;
    let status = response.status;
    let bytes = response
        .bytes(2 * 1024 * 1024)
        .map_err(|error| errors::transport(error, endpoint))?;
    let payload: Value = match serde_json::from_slice(&bytes) {
        Ok(payload) => payload,
        Err(_) if !(200..300).contains(&status) => {
            return Err(errors::upstream(status, &Value::Null, endpoint));
        }
        Err(_) => return Err(errors::protocol(endpoint, "未返回 JSON 对象")),
    };
    if !(200..300).contains(&status) {
        return Err(errors::upstream(status, &payload, endpoint));
    }
    if !payload.is_object() || util::integer(&payload, "code").is_none() {
        return Err(errors::protocol(
            endpoint,
            "缺少明确业务码，未将 HTTP 成功当作业务成功",
        ));
    }
    if util::integer(&payload, "code") != Some(0) {
        return Err(errors::upstream(status, &payload, endpoint));
    }
    let data = payload
        .get("data")
        .filter(|data| data.is_object())
        .ok_or_else(|| errors::protocol(endpoint, "缺少 data 对象"))?;
    Ok(data.clone())
}
