//! 国内gift/compensation是真实一次性奖励，banner/大使只读；不引入国际活跃接口。
use crate::{
    account, endpoints, headers, ledger, network,
    session::{self, Session},
    util,
};
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

const KINDS: [(&str, &str); 2] = [("gift", "新手礼包"), ("compensation", "活动补偿")];

// 上游没有单独的领取资格查询证据；只读入口展示已落库结果，未查询不冒充可领取。
pub fn status() -> Result<Value> {
    let current = session::snapshot()?;
    let personal = account::personal(&current.secret);
    let mut tasks = Vec::new();
    let mut earned = Some(0_i64);
    for (kind, title) in KINDS {
        let saved = ledger::get(&current, &format!("reward-{kind}"))?;
        let result = &saved["result"];
        let done = saved["settled"] == true;
        let credit = done.then(|| util::integer(&result["credit"])).flatten();
        if done {
            earned = earned.zip(credit).map(|(a, b)| a.saturating_add(b));
        }
        tasks.push(
            json!({ "key": kind, "title": title, "group": "运营奖励", "points": credit,
            "done": done, "blocked": !personal, "claimable": null, "eligibility": "unknown", "source": "ledger",
            "checkedAt": saved["at"], "message": result["msg"], "result": result }),
        );
    }
    let settled = !personal || tasks.iter().all(|row| row["done"] == true);
    Ok(
        json!({ "tasks": tasks, "earned": earned, "total": null, "unclaimed": null, "settled": settled,
        "message": if personal { "这里只读已知奖励台账；未领取项目的资格和金额尚未确认，显式领取才会询问官方" } else { "企业账号没有个人运营奖励" } }),
    )
}

pub fn claim_all() -> Result<Value> {
    let current = session::snapshot()?;
    let binding = account::fingerprint(&current.secret);
    let mut results = Vec::new();
    let mut claimed = 0;
    let mut failed = 0;
    let mut credit = Some(0_i64);
    for (kind, _) in KINDS {
        if account::fingerprint(&session::snapshot()?.secret) != binding {
            return Err(Error::http(
                409,
                "奖励领取期间账号身份已变化，未继续另一身份的操作",
            ));
        }
        match claim(kind) {
            Ok(result) => {
                let added = result["success"] == true && result["alreadyCompleted"] != true;
                if added {
                    claimed += 1;
                    credit = credit
                        .zip(util::integer(&result["credit"]))
                        .map(|(a, b)| a.saturating_add(b));
                }
                failed += usize::from(result["outcome"] == "failed");
                results.push(json!({ "key": kind, "ok": result["outcome"] != "failed",
                    "already": result["alreadyCompleted"], "result": result }));
            }
            Err(error) => {
                failed += 1;
                results.push(json!({ "key": kind, "ok": false, "error": error.message }));
            }
        }
    }
    session::check(&current)?;
    let mut result = status()?;
    result["results"] = json!(results);
    result["claimed"] = json!(claimed);
    result["failed"] = json!(failed);
    result["claimedPoints"] = json!(credit);
    result["message"] = json!(if failed > 0 {
        "运营奖励领取有未完成项，请查看逐项结果"
    } else {
        "运营奖励已逐项核对，请查看领取结果"
    });
    Ok(result)
}

pub fn claim(kind: &str) -> Result<Value> {
    let (path, label) = match kind {
        "gift" => ("/billing/meter/claim-gift", "新手礼包"),
        "compensation" => ("/billing/meter/claim-compensation", "活动补偿"),
        _ => return Err(Error::http(400, "奖励类型只能是gift或compensation")),
    };
    let _ = session::ensure(false)?;
    let lock = AccountLock::acquire()?;
    let current = session::ensure_locked(&lock)?;
    if !account::personal(&current.secret) {
        return Ok(
            json!({ "success": false, "outcome": "ineligible", "msg": "企业账号没有个人运营奖励" }),
        );
    }
    let kind = format!("reward-{kind}");
    let old = ledger::get(&current, &kind)?;
    if old["settled"] == true {
        let mut value = old["result"].clone();
        value["success"] = json!(false);
        value["outcome"] = json!("done");
        value["credit"] = json!(0);
        value["alreadyCompleted"] = json!(true);
        value["msg"] = json!(format!("{label}已领取，无需重复提交"));
        return Ok(value);
    }
    let reply = network::raw(
        "POST",
        endpoints::BILL,
        path,
        headers::framework(&current.secret)?,
        Some(&json!({})),
    )?;
    let message = crate::errors::clean(reply.message(), &current.secrets());
    let duplicate = matches!(reply.status, 200 | 400 | 409)
        && (message.contains("已领取") || message.contains("已签到"))
        && !["可能", "未能", "失败"]
            .iter()
            .any(|part| message.contains(part));
    let inactive = ["没有可领取", "暂无可领取", "活动未开启", "无此活动"]
        .iter()
        .any(|part| message.contains(part));
    if !(200..300).contains(&reply.status) && !duplicate && !inactive {
        return Err(crate::errors::upstream(
            reply.status,
            &reply.value,
            &current.secrets(),
        ));
    }
    let success = reply.ok()
        && reply.code() == Some(0)
        && reply.data().is_object()
        && !duplicate
        && !inactive;
    let credit = if success {
        util::integer(&reply.data()["credit"])
    } else if duplicate {
        None
    } else {
        Some(0)
    };
    let outcome = if success {
        "claimed"
    } else if duplicate {
        "done"
    } else if inactive {
        "inactive"
    } else {
        "failed"
    };
    let mut result = json!({ "success": success, "outcome": outcome, "code": reply.code(), "credit": credit, "alreadyCompleted": duplicate,
        "msg": if success { credit.map(|v| format!("{label} +{v} 积分")).unwrap_or_else(|| format!("{label}已领取，金额未提供")) } else { message } });
    session::check(&current)?;
    if ledger::put(
        &current,
        &kind,
        &json!({ "settled": success || duplicate, "at": sdk::now(), "result": result }),
    )
    .is_err()
    {
        util::warning(&mut result, "奖励结果已返回，但台账未保存");
    }
    Ok(result)
}
pub fn banner(current: &Session) -> Result<Value> {
    let mut headers = headers::framework(&current.secret)?;
    headers["X-IDE-Type"] = json!("WorkBuddy");
    headers["X-IDE-Name"] = json!("WorkBuddy");
    headers["X-IDE-Version"] = json!(endpoints::VERSION);
    headers["X-Product"] = json!("WorkBuddy");
    let raw = network::request(
        "GET",
        &current.endpoint()?,
        "/v2/activity/workbuddy/banner",
        headers,
        None,
        &current.secrets(),
    )?;
    if raw.is_null() {
        return Ok(Value::Null);
    }
    let mut value = json!({ "id": raw["activity_id"], "active": util::truth(&raw["activity_online_status"]) && raw["status"] == "None",
        "bannerContent": raw["banner_content"], "link": raw["detail_link"], "level": raw["activity_level"], "startTime": raw["start_time"], "endTime": raw["end_time"], "raw": raw });
    if matches!(
        raw["action"]["type"].as_str(),
        Some("open_url" | "switch_model")
    ) {
        let mut action = json!({ "type": raw["action"]["type"], "label": raw["action"]["label"] });
        for (source, target) in [
            ("url", "url"),
            ("model_id", "modelId"),
            ("fallback_url", "fallbackUrl"),
        ] {
            if let Some(v) = raw["action"].get(source) {
                action[target] = v.clone();
            }
        }
        value["action"] = action;
    }
    session::check(current)?;
    Ok(value)
}
pub fn ambassador(current: &Session) -> Result<Value> {
    let raw = super::request(current, "GET", "/v2/activity/ambassador/status", None)?;
    session::check(current)?;
    Ok(if raw.is_null() {
        Value::Null
    } else {
        json!({ "isAmbassador": util::truth(&raw["isAmbassador"]), "raw": raw })
    })
}
