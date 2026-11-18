//! 每日奖励先读账单，再触发setting_info，最后核对北京时间今日daily_grant。
//! 账单最多5页，每页20条；通知不等于到账，查询失败不能谎报成功。
//! 首次电脑/手机奖励在onboarding独立入口，不能塞进每日调度。

use super::{config, credentials::Credential, ledger, network, session, util};
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

fn bill_day(item: &Value) -> Option<String> {
    let raw = item
        .get("grant_at")
        .or_else(|| item.get("created_at"))?
        .as_str()?;
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|date| util::day(date.timestamp_millis()))
        .or_else(|| raw.get(..10).map(str::to_string))
}
fn grant(item: &Value, today: &str) -> Option<Value> {
    let kind = item["biz_type"].as_str();
    let name = item["event_name"].as_str().unwrap_or("");
    let points = util::integer(&item["points"])?;
    if !(kind == Some("daily_grant") || kind.is_none() && name == "每日积分发放")
        || points <= 0
        || bill_day(item).as_deref() != Some(today)
    {
        return None;
    }
    let at = item["created_at"].as_str().unwrap_or("");
    Some(
        json!({ "name": name, "points": points, "at": at.get(11..19).unwrap_or(""), "expireAt": item["expire_at"] }),
    )
}
fn bills(credential: &Credential, today: &str) -> Result<Vec<Value>> {
    let mut cursor = String::new();
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for _ in 0..5 {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query
            .append_pair("paging.limit", "20")
            .append_pair("type", "all");
        if !cursor.is_empty() {
            query.append_pair("cursor", &cursor);
        }
        let data = network::request(
            "GET",
            &format!("/api/web/points/v2/bills?{}", query.finish()),
            &credential.token,
            &credential.org,
            json!({ "Accept": "application/json" }),
            None,
        )?;
        let rows = data["items"]
            .as_array()
            .ok_or_else(|| Error::http(502, "积分账单缺少items"))?;
        let past = rows
            .iter()
            .any(|item| bill_day(item).is_some_and(|day| day.as_str() < today));
        result.extend(rows.iter().filter_map(|item| grant(item, today)));
        if past || rows.is_empty() {
            break;
        }
        let next = data
            .get("next_cursor")
            .or_else(|| data.get("cursor"))
            .or_else(|| data.pointer("/paging/next_cursor"))
            .or_else(|| data.pointer("/paging/cursor"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let more = data
            .get("has_more")
            .or_else(|| data.pointer("/paging/has_more"))
            .and_then(Value::as_bool)
            .unwrap_or(!next.is_empty());
        if !more || next.is_empty() || !seen.insert(next.to_string()) {
            break;
        }
        cursor = next.into();
    }
    Ok(result)
}

pub fn claim() -> Result<Value> {
    let initial = session::ensure(false)?;
    let lock = AccountLock::acquire()?;
    let current = session::ensure_locked(&lock)?;
    if ledger::binding_of(&current.record) != ledger::binding_of(&initial.record)
        || current.credential.user != initial.credential.user
        || current.credential.org != initial.credential.org
    {
        return Err(Error::http(409, "小浣熊账号已变化，请重新签到"));
    }
    let binding = ledger::binding_of(&current.record);
    let today = util::day(sdk::now());
    let old = ledger::scoped("dailyCheckin", &binding)?;
    if old["completed"] == true
        && old["at"].as_i64().is_some_and(|at| util::day(at) == today)
        && old["report"].is_object()
    {
        let mut report = old["report"].clone();
        report["alreadyCompleted"] = json!(true);
        report["outcome"] = json!("done");
        return Ok(report);
    }
    let result = inner(&current.credential, &today);
    ledger::check_snapshot(&current)?;
    match result {
        Ok(mut report) => {
            let value = json!({ "at": sdk::now(), "completed": report["success"] == true, "report": report });
            if ledger::scoped_put("dailyCheckin", &value, &binding).is_err() {
                util::warning(&mut report, "本次到账核对不受影响，但每日台账未保存");
            }
            ledger::check_binding(&binding)?;
            Ok(report)
        }
        Err(error) => Err(error),
    }
}
fn inner(credential: &Credential, today: &str) -> Result<Value> {
    let previous = match bills(credential, today) {
        Ok(values) => Some(values),
        Err(error) if error.status == Some(401) => return Err(error),
        Err(_) => None,
    };
    let already = previous.as_ref().is_some_and(|rows| !rows.is_empty());
    let mut headers = config::page(false)?;
    headers["X-Raccoon-Language"] = json!("zh");
    let path = if credential.org.is_empty() {
        "/api/web/office/v3/setting_info"
    } else {
        "/api/web/org/user/office/v1/setting_info"
    };
    let settings = network::request(
        "GET",
        path,
        &credential.token,
        &credential.org,
        headers,
        None,
    )?;
    let (grants, error) = if already {
        (previous.unwrap_or_default(), None)
    } else {
        match bills(credential, today) {
            Ok(values) => (values, None),
            Err(error) if error.status == Some(401) => return Err(error),
            Err(error) => (Vec::new(), Some(error.message)),
        }
    };
    let points = grants
        .iter()
        .filter_map(|grant| util::integer(&grant["points"]))
        .fold(0i64, i64::saturating_add);
    let message = if points > 0 {
        format!("今日积分 +{points}")
    } else if error.is_some() {
        "未能核对今日入账".into()
    } else {
        "今日未见积分入账，尚未确认领取".into()
    };
    Ok(
        json!({ "outcome": if already { "done" } else if points > 0 { "claimed" } else if error.is_some() { "failed" } else { "inactive" },
        "credit": (points > 0).then(|| format!("{points} 积分")), "streak": null, "message": message,
        "success": points > 0, "alreadyCompleted": already, "msg": message, "grantsToday": grants,
        "settings": { "popups": settings["point_grant_popups"], "toast": settings["point_grant_toast"] }, "grantsError": error }),
    )
}
