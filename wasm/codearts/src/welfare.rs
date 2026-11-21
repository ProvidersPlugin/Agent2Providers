use crate::{
    binding::Account,
    endpoints::{self, Target},
    errors, signer, util, welfare_read,
};
use agent2api_wasm_sdk::{self as sdk, json, Result, Value};

const LEDGER: &str = "codeartsWelfare";

fn request(account: &Account, path: &str, body: &Value) -> Result<Value> {
    account.unchanged()?;
    account.readable()?;
    let target = Target {
        origin: endpoints::BASE,
        path: path.into(),
        query: String::new(),
        label: "福利领取",
        include_host: false,
        domainless: false,
        headers: vec![
            ("Content-Type".into(), "application/json".into()),
            ("Agent-Type".into(), "PromptCenter".into()),
            ("X-Language".into(), endpoints::LANGUAGE.into()),
        ],
    };
    let bytes = body.to_string();
    let headers = signer::signed_request("POST", &target, &account.credential, bytes.as_bytes())?;
    let response = sdk::http::open_raw_timeout(
        "POST",
        &target.url(),
        headers,
        Some(bytes.as_bytes()),
        20_000,
    )?;
    let status = response.status;
    let bytes = response.bytes(256 * 1024)?;
    if status != 200 {
        let value = errors::parse_body(&bytes);
        return Err(errors::upstream(
            status,
            "福利领取",
            &value,
            &account.credential.secrets(),
        ));
    }
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| errors::protocol("活动领取没有返回完整回应"))?;
    if value["code"].as_i64() != Some(0) {
        return Err(errors::upstream(
            status,
            "福利领取",
            &value,
            &account.credential.secrets(),
        ));
    }
    account.unchanged()?;
    Ok(value["data"].clone())
}

fn key(value: &Value) -> Result<String> {
    let key = match value {
        Value::String(id) => id.clone(),
        Value::Number(id) => id.to_string(),
        _ => return Err(errors::protocol("活动编号无效")),
    };
    if key.is_empty() || key.len() > 256 {
        return Err(errors::protocol("活动编号无效"));
    }
    Ok(key)
}

fn select(preview: &Value, newbie: bool) -> Vec<Value> {
    preview["campaigns"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| {
            if newbie {
                row["kind"] == "NEW_USER_REGISTER"
            } else {
                row["kind"] == "USER_LOGIN" && row["benefitUnit"] == "CREDIT"
            }
        })
        .cloned()
        .collect()
}

fn amount(rows: &[Value], confirmed_only: bool) -> Value {
    let mut total = 0f64;
    for row in rows
        .iter()
        .filter(|row| !confirmed_only || row["done"] == true)
    {
        let Some(number) = row["points"]
            .as_f64()
            .filter(|number| number.is_finite() && *number >= 0.0)
        else {
            return Value::Null;
        };
        total += number;
        if !total.is_finite() {
            return Value::Null;
        }
    }
    if rows.is_empty() {
        Value::Null
    } else {
        json!(total)
    }
}

fn view(preview: &Value, newbie: bool) -> Result<Value> {
    let rows = select(preview, newbie).iter().map(|row| Ok(json!({
        "key": key(&row["id"])? , "title": row["title"], "group": if newbie { "新人注册礼" } else { "每日福利" },
        "points": row["benefitAmount"], "unit": row["benefitUnit"], "done": row["observedConfirmed"],
            "blocked": row["upstreamClaimable"] != true && row["status"] != "CLAIMED" && row["observedConfirmed"] != true
                || row["kind"] == "NEW_USER_REGISTER" && row["benefitUnit"] != "CREDIT",
        "status": row["status"],
    }))).collect::<Result<Vec<_>>>()?;
    let unclaimed = rows
        .iter()
        .filter(|row| row["done"] != true && row["blocked"] != true)
        .count();
    Ok(
        json!({"provider":"codearts", "tasks": rows, "unclaimed": unclaimed,
        "earned": amount(&rows, true), "total": amount(&rows, false),
        "settled": !rows.is_empty() && rows.iter().all(|row| row["done"] == true),
        "note":"福利到账的是套餐赠送积分，不增加福利模型的 Token 池"}),
    )
}

fn save_memory(value: &Value, account: &Account) -> Result<()> {
    account.unchanged()?;
    if value["settled"] == true {
        let memory = json!({"at":util::now()?, "tasks":value["tasks"], "earned":value["earned"], "total":value["total"]});
        let digest = util::digest(&[memory.to_string().as_bytes()]);
        // 公开结清记忆只含安全展示字段；绑定和摘要单独留在本账号私有台账。
        sdk::host(
            "ledger.put",
            json!({"kind":"codeartsOnboardingBinding", "value":{
                "version":1, "binding":account.binding, "digest":digest
            }}),
        )?;
        sdk::host("ledger.put", json!({"kind":"onboarding", "value":memory}))?;
    }
    account.unchanged()
}

pub fn status(newbie: bool) -> Result<Value> {
    status_with_refresh(newbie, false)
}

pub fn status_with_refresh(newbie: bool, refresh: bool) -> Result<Value> {
    let account = Account::current()?;
    if newbie && !refresh {
        let memory = sdk::host("ledger.get", json!({"kind":"onboarding"}))?;
        let binding = sdk::host("ledger.get", json!({"kind":"codeartsOnboardingBinding"}))?;
        if binding["version"] == 1
            && binding["binding"] == json!(account.binding)
            && binding["digest"].as_str()
                == Some(util::digest(&[memory.to_string().as_bytes()]).as_str())
            && memory["tasks"].as_array().is_some_and(|rows| {
                !rows.is_empty()
                    && rows.len() <= 256
                    && rows
                        .iter()
                        .all(|row| row["done"] == true && row["key"].is_string())
            })
        {
            let result = json!({"provider":"codearts", "tasks":memory["tasks"], "earned":memory["earned"],
                "total":memory["total"], "unclaimed":0, "settled":true, "settledAt":memory["at"]});
            validate_memory(&result, &account)?;
            account.unchanged()?;
            return Ok(result);
        }
    }
    let result = view(&welfare_read::preview(newbie)?, newbie)?;
    account.unchanged()?;
    if newbie {
        save_memory(&result, &account)?;
    }
    Ok(result)
}

fn validate_memory(value: &Value, account: &Account) -> Result<()> {
    let secrets = account.credential.secrets();
    if value.to_string().len() > 64 * 1024
        || errors::contains_secret(&value.to_string(), &secrets)
        || value["tasks"].as_array().into_iter().flatten().any(|row| {
            row.as_object().is_none_or(|fields| {
                fields.keys().any(|key| {
                    ![
                        "key", "title", "group", "points", "unit", "done", "blocked", "status",
                    ]
                    .contains(&key.as_str())
                })
            })
        })
    {
        return Err(errors::protocol(
            "CodeArts 既有新手礼记录不是当前安全展示格式，未公开旧记录",
        ));
    }
    Ok(())
}

fn fresh(day: &str) -> Value {
    json!({"version":2,"day":day,"attempts":0,"lastAttemptMs":0,"accepted":false,"campaigns":{}})
}

fn ledger(now: i64) -> Result<Value> {
    let day = chrono::DateTime::from_timestamp_millis(now + 8 * 60 * 60 * 1000)
        .ok_or_else(|| errors::protocol("签到时钟无效"))?
        .format("%Y-%m-%d")
        .to_string();
    let stored = sdk::host("ledger.get", json!({"kind":LEDGER}))?;
    if stored.is_null() {
        return Ok(fresh(&day));
    }
    let stored_day = stored["day"]
        .as_str()
        .ok_or_else(|| errors::protocol("领取台账缺少日期"))?;
    if stored["version"] != 2
        || !stored["campaigns"].is_object()
        || !stored["attempts"]
            .as_u64()
            .is_some_and(|number| number <= 6)
        || chrono::NaiveDate::parse_from_str(stored_day, "%Y-%m-%d").is_err()
    {
        return Err(errors::protocol("领取台账格式无效，未覆盖或丢弃旧去重凭据"));
    }
    if stored_day > day.as_str() {
        return Err(errors::protocol("系统日期早于领取记录，请纠正时间"));
    }
    Ok(if stored_day == day {
        stored
    } else {
        let mut next = fresh(&day);
        // 日切只重置每日计数，未确认的领取机会不能因过午夜获得第二次发送资格。
        let mut unresolved = serde_json::Map::new();
        for (key, progress) in stored["campaigns"].as_object().into_iter().flatten() {
            if progress["confirmed"] != true
                && (progress["claimPending"] == true
                    || progress["confirmPending"] == true
                    || progress["claimed"] == true)
            {
                unresolved.insert(key.clone(), progress.clone());
            }
        }
        next["campaigns"] = Value::Object(unresolved);
        next
    })
}

fn save(ledger: &Value, account: &Account) -> Result<()> {
    account.unchanged()?;
    sdk::host("ledger.put", json!({"kind":LEDGER,"value":ledger}))?;
    account.unchanged()
}

pub fn claim(newbie: bool, manual: bool) -> Result<Value> {
    let _lock = sdk::AccountLock::acquire()?;
    let account = Account::current()?;
    account.readable()?;
    let before = welfare_read::preview(newbie)?;
    account.unchanged()?;
    let selected = select(&before, newbie);
    let initial = view(&before, newbie)?;
    if selected.is_empty() || selected.iter().all(|row| row["observedConfirmed"] == true) {
        if newbie {
            save_memory(&initial, &account)?;
            return Ok(initial);
        }
        return Ok(report(
            if selected.is_empty() {
                "inactive"
            } else {
                "done"
            },
            if selected.is_empty() {
                "暂无可领取活动"
            } else {
                "已领取并由官方确认"
            },
            None,
        ));
    }
    let now = util::now()?;
    let mut ledger = ledger(now)?;
    if !manual
        && (ledger["attempts"].as_u64().unwrap_or(0) >= 6
            || ledger["lastAttemptMs"]
                .as_i64()
                .is_some_and(|at| at > 0 && now.saturating_sub(at) < 600_000))
    {
        return Ok(report(
            "inactive",
            "每日福利仍在领取间隔内，未重复提交",
            None,
        ));
    }
    let mut claimed = Vec::new();
    let mut uncertain = false;
    for campaign in &selected {
        if campaign["observedConfirmed"] == true
            || campaign["upstreamClaimable"] != true && campaign["status"] != "CLAIMED"
            || newbie && campaign["benefitUnit"] != "CREDIT"
        {
            continue;
        }
        let key = key(&campaign["id"])?;
        if ledger["campaigns"][&key].is_null() {
            ledger["campaigns"][&key] = json!({"idempotentKey":util::hex(&util::random::<16>()?),
                "claimPending":false,"confirmPending":false,"claimed":false,"confirmed":false});
        }
        if campaign["status"] != "CLAIMED" {
            if ledger["campaigns"][&key]["claimPending"] == true
                || ledger["campaigns"][&key]["claimed"] == true
                || ledger["campaigns"][&key]["confirmed"] == true
            {
                uncertain = true;
                continue;
            }
            let id = ledger["campaigns"][&key]["idempotentKey"]
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= 128)
                .ok_or_else(|| errors::protocol("活动去重编号缺失"))?
                .to_string();
            ledger["attempts"] = json!(ledger["attempts"]
                .as_u64()
                .unwrap_or(0)
                .saturating_add(1)
                .min(6));
            ledger["lastAttemptMs"] = json!(util::now()?);
            ledger["accepted"] = json!(false);
            ledger["campaigns"][&key]["claimPending"] = json!(true);
            save(&ledger, &account)?;
            let value = request(
                &account,
                "/v1/ops/claim",
                &json!({"campaignId":campaign["id"],"idempotentKey":id,"channel":"IDE"}),
            )?;
            if self::key(&value["campaignId"])? != key {
                return Err(errors::protocol("领取回应的活动编号不一致，未继续确认"));
            }
            ledger["campaigns"][&key]["claimed"] = json!(true);
            ledger["campaigns"][&key]["claimPending"] = json!(false);
            save(&ledger, &account)?;
        }
        if ledger["campaigns"][&key]["confirmPending"] == true
            || ledger["campaigns"][&key]["confirmed"] == true
        {
            uncertain = true;
            continue;
        }
        ledger["campaigns"][&key]["confirmPending"] = json!(true);
        save(&ledger, &account)?;
        request(
            &account,
            "/v1/ops/confirm",
            &json!({"campaignId":campaign["id"]}),
        )?;
        claimed.push(key);
    }
    let after = welfare_read::preview(newbie)?;
    account.unchanged()?;
    let result = view(&after, newbie)?;
    let confirmed = select(&after, newbie);
    let mut points = Some(0f64);
    let mut confirmed_now = 0usize;
    for key in &claimed {
        let row = confirmed.iter().find(|row| {
            self::key(&row["id"]).ok().as_deref() == Some(key.as_str())
                && row["kind"]
                    == if newbie {
                        "NEW_USER_REGISTER"
                    } else {
                        "USER_LOGIN"
                    }
                && row["observedConfirmed"] == true
        });
        if let Some(row) = row {
            confirmed_now += 1;
            ledger["campaigns"][key]["confirmed"] = json!(true);
            ledger["campaigns"][key]["confirmPending"] = json!(false);
            points = points
                .zip(row["benefitAmount"].as_f64())
                .map(|(total, amount)| total + amount);
        } else {
            uncertain = true;
        }
    }
    if uncertain
        && !selected.is_empty()
        && selected.iter().all(|original| {
            confirmed.iter().any(|row| {
                row["id"] == original["id"]
                    && row["kind"] == original["kind"]
                    && row["observedConfirmed"] == true
            })
        })
    {
        // 未确认发送只靠本次真实回读收敛，不通过重发旧一次性请求判成功。
        uncertain = false;
    }
    ledger["accepted"] = json!(!uncertain && confirmed_now > 0);
    save(&ledger, &account)?;
    if newbie {
        save_memory(&result, &account)?;
        let mut result = result;
        result["claimed"] = json!(confirmed_now);
        result["failed"] = json!(usize::from(uncertain));
        result["message"] = json!(if uncertain {
            "领取结果尚未获官方确认；本次不重复提交"
        } else if confirmed_now == 0 {
            "暂无新增可领取新手礼"
        } else {
            "新手礼已由官方确认"
        });
        return Ok(result);
    }
    if uncertain {
        return Ok(report(
            "failed",
            "领取结果尚未获官方确认；再次执行只查询、不重复提交",
            None,
        ));
    }
    if confirmed_now == 0 {
        return Ok(report(
            if !confirmed.is_empty() && confirmed.iter().all(|row| row["observedConfirmed"] == true)
            {
                "done"
            } else {
                "inactive"
            },
            "暂无新增可领取活动",
            None,
        ));
    }
    Ok(report(
        "claimed",
        "官方已确认到账",
        points
            .filter(|points| points.is_finite())
            .map(|points| format!("{points} 积分")),
    ))
}

fn report(outcome: &str, message: &str, credit: Option<String>) -> Value {
    json!({"outcome":outcome,"message":message,"credit":credit,"streak":null})
}
