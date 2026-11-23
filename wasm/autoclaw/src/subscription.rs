//! 国内订阅用 subscribe-info，不能误用国际 Stripe 的 oversea-subscribe-info。
//! 保留原生有限深搜与原始到期形态，读不到额度留空。
use agent2api_wasm_sdk::{json, Result, Value};
use super::{account, client, util};

pub fn query(secret: &Value) -> Result<Value> {
    let mut body = json!({ "source_id": "autoclaw" });
    let device = account::device(secret);
    if !device.is_empty() { body["device_id"] = json!(device); }
    let payload = client::post("/agentpay/v1/assistant/subscribe-info", &account::access(secret), &body, "订阅查询")?;
    client::business(&payload, "查询订阅")?;
    let data = &payload["data"];
    if data.is_null() { return Ok(Value::Null); }
    Ok(normalize(data))
}
fn find<'a>(data: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let nested = ["subscribeInfo", "subscribe", "data", "memberInfo", "vipInfo"].iter()
        .find_map(|key| data.get(*key).filter(|value| value.is_object()));
    let mut pending = vec![data];
    if let Some(nested) = nested { pending.push(nested); }
    let mut visited = 0;
    while let Some(value) = pending.pop() {
        visited += 1;
        if visited > 200 { break; }
        for (key, value) in value.as_object().into_iter().flatten() {
            if keys.contains(&key.as_str()) && !value.is_null() && value != "" { return Some(value); }
            if value.is_object() { pending.push(value); }
        }
    }
    None
}
fn normalize(data: &Value) -> Value {
    let text = |keys: &[&str]| find(data, keys).map(util::scalar).unwrap_or_default();
    let number = |keys: &[&str]| find(data, keys).and_then(util::number);
    json!({ "planName": text(&["productName", "planName", "memberName", "subscribeName", "vipName"]),
        "status": text(&["status", "subscribeStatus", "memberStatus"]),
        "expireAt": find(data, &["expireAt", "expireTime", "expiredAt", "endTime", "validEndTime"]).cloned().unwrap_or(Value::Null),
        "remainQuota": number(&["remainQuota", "remainingQuota", "remainCredits", "availableCredits", "leftQuota"]),
        "totalQuota": number(&["totalQuota", "credits", "totalCredits"]) })
}
