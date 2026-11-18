//! 出站唯一入口：五类枚举 GET、精确 .net、仅普通用户 Session；不发整份 Cookie。
//! 30 秒总预算来自本包保守资源策略。正文需 HTTP 成功 + code=0 + data 对象。

use crate::{
    budget::Budget,
    endpoints::{self, ReadEndpoint},
    errors, strict_json,
};
use agent2api_wasm_sdk::{http, json, Result, Value};

const MAX_RESPONSE: usize = 2 * 1024 * 1024;

pub struct Document {
    pub data: Value,
    pub bytes: usize,
}

pub fn get(
    session: &str,
    endpoint: ReadEndpoint,
    query: &[(&str, String)],
    budget: &Budget,
) -> Result<Document> {
    let response = http::open_timeout(
        "GET",
        &endpoint.url(query)?,
        json!({
            "Cookie": format!("{}={session}", endpoints::SESSION_COOKIE),
            "Origin": endpoints::ORIGIN,
            "Referer": format!("{}/console/", endpoints::ORIGIN),
            "Accept": "application/json"
        }),
        None,
        budget.remaining()?,
    )
    .map_err(|error| errors::transport(error, endpoint))?;
    let status = response.status;
    let bytes = response
        .bytes(MAX_RESPONSE)
        .map_err(|error| errors::transport(error, endpoint))?;
    budget.remaining()?;
    let payload = strict_json::parse(&bytes);
    let business = payload
        .as_ref()
        .and_then(|payload| payload["code"].as_i64());
    if !(200..300).contains(&status) {
        return Err(errors::upstream(status, business, endpoint));
    }
    let payload = payload.ok_or_else(|| errors::protocol(endpoint, "非 JSON、重复键或嵌套超限"))?;
    if business != Some(0) {
        return Err(if business.is_some() {
            errors::upstream(status, business, endpoint)
        } else {
            errors::protocol(endpoint, "缺少明确整数业务码")
        });
    }
    let data = payload
        .get("data")
        .filter(|data| data.is_object())
        .ok_or_else(|| errors::protocol(endpoint, "缺少 data 对象"))?;
    Ok(Document {
        data: data.clone(),
        bytes: bytes.len(),
    })
}
