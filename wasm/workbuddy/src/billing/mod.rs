//! 国内积分既有五键保持不变，追加资源包/套餐；网络与账号均经宿主。
pub mod checkin;
mod commodity;
mod resources;
pub mod rewards;
mod usage;

use crate::{
    headers, network,
    session::{self, Session},
};
use agent2api_wasm_sdk::{json, Result, Value};
pub fn request(current: &Session, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
    let empty = json!({});
    let body = if matches!(method, "GET" | "HEAD") {
        None
    } else {
        Some(body.unwrap_or(&empty))
    };
    let mut headers = headers::framework(&current.secret)?;
    headers["Accept-Language"] = json!("zh-CN");
    network::request(
        method,
        &current.endpoint()?,
        path,
        headers,
        body,
        &current.secrets(),
    )
}
pub fn query() -> Result<Value> {
    let current = session::ensure(false)?;
    let usage = usage::query_usage(&current)?;
    let mut value = usage::credits_summary_from(&usage);
    let packs = usage::credit_packs_from(&usage);
    if !packs.is_empty() {
        value["credits"] = json!(packs);
    }
    if let Some(subscription) = usage::subscription_of(&usage) {
        value["subscription"] = subscription;
    }
    session::check(&current)?;
    value["quotaSources"] = json!([{ "id": "credits", "label": "可用积分", "provider": "workbuddy", "unit": "积分",
        "remaining": value["totalLeft"], "total": null, "used": null, "unlimited": value["unlimited"] }]);
    Ok(value)
}
pub fn sources(report: &Value) -> Value {
    report["quotaSources"]
        .as_array()
        .cloned()
        .map(Value::Array)
        .unwrap_or_else(|| json!([]))
}
