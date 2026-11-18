//! project/订阅只走 prod。先 loadCodeAssist，无 project 才用上游默认档 onboard 一次。
//! 不填造 tenant、不按套餐猜资格，不轮询后台任务；未完成留给下次明确刷新。

use super::{
    account, client, credentials,
    endpoints::{self, Plane},
    util,
};
use agent2api_wasm_sdk::{json, AccountLock, Error, Result, Value};

pub fn load(token: &str) -> Result<Value> {
    client::post(
        &endpoints::method(endpoints::PROD, "loadCodeAssist"),
        endpoints::headers(token, "", Plane::Admin, false)?,
        &json!({ "metadata": { "ideType": "ANTIGRAVITY" } }),
    )?
    .accepted()
}

fn project_value(value: &Value) -> String {
    value
        .as_str()
        .or_else(|| value["id"].as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

pub fn tier(body: &Value) -> String {
    [
        "/paidTier/id",
        "/currentTier/id",
        "/paidTier/name",
        "/currentTier/name",
    ]
    .iter()
    .find_map(|p| {
        body.pointer(p)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    })
    .unwrap_or("")
    .to_string()
}

pub fn discover(token: &str, check: impl Fn() -> Result<()>) -> Result<String> {
    check()?;
    let body = load(token)?;
    check()?;
    let project = project_value(&body["cloudaicompanionProject"]);
    if !project.is_empty() {
        return util::header(&project, 1024);
    }
    let tier = body["allowedTiers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|tier| tier["isDefault"] == true)
        .find_map(|tier| tier["id"].as_str().map(str::trim).filter(|v| !v.is_empty()))
        .unwrap_or("legacy-tier");
    check()?;
    let reply = client::post(
        &endpoints::method(endpoints::PROD, "onboardUser"),
        endpoints::headers(token, "", Plane::Admin, false)?,
        &json!({ "tierId": tier, "metadata": { "ideType": "ANTIGRAVITY" } }),
    )?
    .accepted()?;
    check()?;
    let project = project_value(&reply["response"]["cloudaicompanionProject"]);
    if reply["done"] != true || project.is_empty() {
        return Err(Error::http(
            502,
            "Antigravity 项目尚未开通或没有返回项目标识，请稍后刷新；未编造项目或绕过资格",
        ));
    }
    util::header(&project, 1024)
}

/// 已拿账号锁时使用，避免重复锁。无项目不是目录的致命错误，但图片必须有真实项目。
pub fn locked(record: Value, required: bool) -> Result<Value> {
    let secret = credentials::parse(&record["secret"])?;
    if !util::text(&secret, &["projectId"]).is_empty() {
        return Ok(record);
    }
    let result = discover(&credentials::token(&secret)?, || {
        account::unchanged(&record["secret"])
    });
    account::unchanged(&record["secret"])?;
    match result {
        Ok(project) => account::update(record, &json!({ "projectId": project })),
        Err(error) if required => Err(error),
        Err(_) => Ok(record),
    }
}

pub fn ensure(record: Value, required: bool) -> Result<Value> {
    let _lock = AccountLock::acquire()?;
    account::unchanged(&record["secret"])?;
    locked(record, required)
}
