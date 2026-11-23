//! 国际积分查询保留kind/unlimited/totalLeft/planLeft/bonusLeft，追加套餐与资源包。
//! 仅国际账号出口，原生共享商品码按国际返回值解读；不请求国内奖励站点。
mod commodity;
pub mod operations;
mod resources;
mod usage;

use crate::{
    headers, network,
    session::{self, Session},
};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub(super) struct QueryBudget(i64);

impl QueryBudget {
    fn new() -> Self { Self(sdk::now().saturating_add(55_000)) }

    pub(super) fn raw(&self, current: &Session, method: &str, path: &str, body: Option<&Value>) -> Result<network::Reply> {
        let remaining = self.0.saturating_sub(sdk::now());
        if remaining < 1000 {
            return Err(Error::http(408, "国际积分查询已到本轮时间上限，保留上次读数")
                .with_code("usage_timeout"));
        }
        let empty = json!({});
        let body = if matches!(method, "GET" | "HEAD") { None } else { Some(body.unwrap_or(&empty)) };
        let mut headers = headers::framework(&current.secret)?;
        headers["Accept-Language"] = json!("en");
        network::raw_timeout(method, &current.endpoint()?, path, headers, body, (remaining as u64).min(20_000))
    }

    pub(super) fn request(&self, current: &Session, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        self.raw(current, method, path, body)?.checked(&current.secrets())
    }
}

pub fn request(current: &Session, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
    raw(current, method, path, body)?.checked(&current.secrets())
}
pub fn raw(
    current: &Session,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<network::Reply> {
    let empty = json!({});
    let body = if matches!(method, "GET" | "HEAD") {
        None
    } else {
        Some(body.unwrap_or(&empty))
    };
    let mut headers = headers::framework(&current.secret)?;
    headers["Accept-Language"] = json!("en");
    network::raw_timeout(method, &current.endpoint()?, path, headers, body, 20_000)
}
pub fn query() -> Result<Value> {
    let budget = QueryBudget::new();
    let current = session::ensure(false)?;
    let usage = usage::query_usage(&current, &budget)?;
    let mut value = usage::credits_summary_from(&usage);
    let packs = usage::credit_packs_from(&usage);
    if !packs.is_empty() {
        value["credits"] = json!(packs);
    }
    if let Some(subscription) = usage::subscription_of(&usage) {
        value["subscription"] = subscription;
    }
    value["quotaSources"] = json!([{ "id": "credits", "label": "国际可用积分", "provider": "workbuddy-intl", "unit": "积分",
        "remaining": value["totalLeft"], "total": null, "used": null, "unlimited": value["unlimited"] }]);
    session::check(&current)?;
    Ok(value)
}
pub fn sources(report: &Value) -> Value {
    report["quotaSources"]
        .as_array()
        .cloned()
        .map(Value::Array)
        .unwrap_or_else(|| json!([]))
}
