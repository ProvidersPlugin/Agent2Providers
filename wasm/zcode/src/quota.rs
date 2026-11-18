//! 账单先于模型请求读取以保留原始错误报文；只用真实1005补充说明，不切套餐或领取。
use agent2api_wasm_sdk::{self as sdk, Error, Result, Value};
use super::{account, claim_state::Ledger, config, errors, network, prompt, usage_billing, usage_sources};

fn time(value: &Value) -> Option<i64> {
    network::number(value).filter(|value| *value > 0.0 && *value < (1_i64 << 53) as f64)
        .map(|value| if value < 100_000_000_000.0 { (value * 1000.0) as i64 } else { value as i64 })
}
fn named_time(at: i64) -> String {
    network::clock_label(at).map(|time| format!("{time}（北京时间）")).unwrap_or_else(|| "官方有效时间".into())
}
fn unavailable(code: &str, message: String, reset: Option<i64>) -> Error {
    let mut error = Error::http(429, message).with_code(code).with_reset(reset);
    error.upstream_code = Some(1005);
    error
}
fn ended_receipt(record: &Value, now: i64) -> Option<i64> {
    let current = account::record().ok()?;
    if current["id"] != record["id"] || current["secret"] != record["secret"] { return None; }
    let ledger = Ledger::load(record).ok()?;
    ledger.plans.values().filter_map(|saved| {
        if saved.claimed_at.is_none_or(|at| at <= 0) && saved.owned.is_none() { return None; }
        let at = saved.claim_ends_at.or_else(|| saved.owned.as_ref().and_then(|owned| owned.ends_at))
            .or(saved.plan.ends_at)?;
        time(&Value::from(at))
    }).filter(|at| *at <= now).max()
}

pub fn snapshot(record: &Value) -> Result<Option<Value>> {
    const TTL: i64 = 30_000;
    let binding = config::model_verification_binding(record);
    let version = prompt::client_version()?;
    let before = config::transient("model-balance")?;
    let now = sdk::now();
    let at = before.value["at"].as_i64().unwrap_or(0);
    if before.compatible && before.value["binding"] == binding && before.value["version"] == version
        && at > 0 && at <= now && now.saturating_sub(at) < TTL {
        return Ok(before.value["data"].is_object().then(|| before.value["data"].clone()));
    }
    let data = match usage_billing::read_version(record, &version) {
        Ok(data) => Some(data),
        Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
        Err(_) => None,
    };
    let current = account::record()?;
    if current["id"] != record["id"] || current["secret"] != record["secret"] { return Ok(None); }
    let value = sdk::json!({ "binding": binding, "version": version, "at": sdk::now(), "data": data });
    match config::put_transient("model-balance", &before, value, TTL) {
        Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
        _ => {}
    }
    Ok(data)
}

pub fn failure(status: u16, body: &Value, record: &Value, model: &str, data: Option<&Value>) -> Result<Error> {
    let original = errors::failure(status, body);
    if status == 401 || errors::code(body) != Some(1005) || account::plan(&record["secret"]) != account::START {
        return Ok(original);
    }
    let Some(data) = data else { return Ok(original); };
    let current = account::record()?;
    if current["id"] != record["id"] || current["secret"] != record["secret"] { return Ok(original); }
    let (Some(plans), Some(balances)) = (data["plans"].as_array(), data["balances"].as_array()) else {
        return Ok(original);
    };
    let now = time(&data["server_time"]).unwrap_or_else(sdk::now);
    if plans.is_empty() && balances.is_empty() {
        let detail = ended_receipt(record, now).map(|at|
            format!("最近一次已领取的活动套餐在{}到期。", named_time(at))).unwrap_or_default();
        return Ok(unavailable("zcode_start_plan_unavailable", format!(
            "ZCode 套餐池当前没有有效套餐（1005）。{detail}签到或领取成功不代表全天可用，请核对套餐的生效时段；未切换到付费通道。"
        ), None));
    }
    let sources = usage_sources::billing(&data);
    let matching = sources.iter().filter(|source| source["models"].as_array().is_some_and(|models|
        models.iter().any(|name| name.as_str().is_some_and(|name| name.eq_ignore_ascii_case(model)))))
        .collect::<Vec<_>>();
    if matching.is_empty() && sources.iter().any(|source|
        source["models"].as_array().is_none_or(Vec::is_empty)) { return Ok(original); }
    let active = matching.iter().copied().filter(|source|
        time(&source["startsAt"]).is_none_or(|at| at <= now)
            && time(&source["endsAt"]).is_none_or(|at| at > now)).collect::<Vec<_>>();
    if active.iter().any(|source| source["unlimited"] == true
        || network::number(&source["remaining"]).is_none_or(|remaining| remaining > 0.0)) {
        return Ok(original);
    }
    let future = matching.iter().copied().filter_map(|source| {
        let start = time(&source["startsAt"])?;
        (start > now && time(&source["endsAt"]).is_none_or(|end| end > start)
            && (source["unlimited"] == true || network::number(&source["remaining"]).is_none_or(|left| left > 0.0)))
            .then_some(start)
    }).min();
    if active.is_empty() {
        if let Some(at) = future {
            return Ok(unavailable("zcode_start_plan_pending", format!(
                "ZCode 套餐池的 {model} 额度尚未生效（1005），官方生效时间为{}；未提前重发或切换到付费通道。", named_time(at)
            ), Some(at)));
        }
        if matching.is_empty() || matching.iter().any(|source| time(&source["endsAt"]).is_some_and(|at| at <= now)) {
            return Ok(unavailable("zcode_start_model_unavailable", format!(
                "ZCode 套餐池当前没有为 {model} 生效的额度（1005），其他模型或已到期活动包的额度不能用于这次请求；未切换到付费通道。"
            ), None));
        }
        return Ok(original);
    }
    let reset = active.iter().filter_map(|source| time(&source["resetsAt"]))
        .filter(|at| *at > now).chain(future).min();
    let detail = reset.map(|at| format!("官方下一次恢复时间为{}。", named_time(at))).unwrap_or_default();
    Ok(unavailable("zcode_start_model_exhausted", format!(
        "ZCode 套餐池的 {model} 当前生效额度已用完（1005）。{detail}未把其他模型标记为额度耗尽，也未切换到付费通道。"
    ), reset))
}
