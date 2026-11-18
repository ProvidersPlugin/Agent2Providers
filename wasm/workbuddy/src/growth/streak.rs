//! 补签→7/14/28档兑换→最多20次抽奖；每步只认已知状态，不确定结果不换新幂等键重发。
use super::GrowthAccount;
use crate::{ledger, util};
use agent2api_wasm_sdk::{self as sdk, json, Value};

fn get(account: &GrowthAccount, path: &str) -> Result<Value, String> {
    let reply = super::chat_call(account, "GET", path, None, &[])?;
    if !reply.ok() {
        return Err(format!("连登查询失败：{}", reply.msg()));
    }
    Ok(reply.data().clone())
}
fn post(account: &GrowthAccount, path: &str, body: &Value) -> Result<Value, String> {
    let reply = super::chat_call(account, "POST", path, Some(body), &[])?;
    if !reply.ok() {
        return Err(format!("连登操作未完成：{}", reply.msg()));
    }
    Ok(reply.data().clone())
}
/// 同一次待确认消费复用client_token；无确认结果不凭空开始下一次抽奖。
fn idempotent(
    account: &GrowthAccount,
    kind: &str,
    path: &str,
    body: &Value,
) -> Result<Value, String> {
    let old = ledger::get(&account.session, kind).map_err(|e| e.message)?;
    let token = if old["pending"] == true {
        util::text(&old, &["clientToken"])
    } else {
        util::uuid().map_err(|e| e.message)?
    };
    if token.is_empty() {
        return Err("连登幂等记录无效，未继续消费奖励".into());
    }
    let mut body = body.clone();
    body["client_token"] = json!(token);
    ledger::put(
        &account.session,
        kind,
        &json!({ "pending": true, "clientToken": token, "at": sdk::now() }),
    )
    .map_err(|e| e.message)?;
    let result = post(account, path, &body)?;
    if let Err(error) = ledger::put(
        &account.session,
        kind,
        &json!({ "pending": false, "at": sdk::now(), "result": result }),
    ) {
        let mut known = result;
        if !known.is_object() {
            known = json!({ "result": known });
        }
        known["warning"] = json!(format!(
            "官方结果已返回，但消费台账未保存：{}",
            error.message
        ));
        known["stopAfterKnownResult"] = json!(true);
        return Ok(known);
    }
    Ok(result)
}
pub fn run(account: &GrowthAccount) -> Result<Value, String> {
    let mut steps = Vec::new();
    let yesterday = util::day(sdk::now().saturating_sub(86_400_000));
    let heatmap = get(account, "/activity/growth/heatmap");
    match heatmap {
        Ok(map) if map["cells"].as_array().is_some_and(|rows| rows.iter().any(|row| row["date"].as_str().and_then(|s| s.get(..10)) == Some(yesterday.as_str()) && util::integer(&row["score"]) == Some(0))) => {
            match get(account, "/activity/growth/streak") {
                Ok(full) if util::integer(&full["makeup_cards"]["balance"]).is_some_and(|v| v > 0) => {
                    match post(account, "/activity/growth/makeup-cards/use", &json!({ "target_date": yesterday })) {
                        Ok(_) => steps.push(json!({ "outcome": "makeup", "message": format!("已使用补签卡补签{yesterday}") })),
                        Err(error) => steps.push(json!({ "outcome": "failed", "message": error })),
                    }
                }
                Ok(_) => steps.push(json!({ "outcome": "skip", "message": "昨日漏签但无补签卡" })),
                Err(error) => steps.push(json!({ "outcome": "failed", "message": error })),
            }
        }
        Ok(_) => {}, Err(error) => steps.push(json!({ "outcome": "failed", "message": error })),
    }
    if let Err(error) = account.check() {
        steps.push(json!({ "outcome": "failed", "message": error }));
        return Ok(summary(account, steps));
    }
    match get(account, "/activity/growth/streak") {
        Ok(full) => {
            for tier in ["7d", "14d", "28d"] {
                if full["redemption_status"][format!("tier_{tier}_status")] != "unlocked" {
                    continue;
                }
                let result = idempotent(
                    account,
                    &format!("streak-redeem-{tier}"),
                    "/activity/growth/redeem",
                    &json!({ "tier": tier }),
                );
                match result { Ok(reward) => steps.push(json!({ "outcome": "redeem", "message": format!("已兑换{tier}档"), "reward": reward })),
                Err(error) => steps.push(json!({ "outcome": "failed", "message": error })) }
            }
        }
        Err(error) => steps.push(json!({ "outcome": "failed", "message": error })),
    }
    if let Err(error) = account.check() {
        steps.push(json!({ "outcome": "failed", "message": error }));
        return Ok(summary(account, steps));
    }
    match get(account, "/activity/growth/lottery/summary") {
        Ok(value) => {
            let Some(count) = util::integer(&value["chances"]).filter(|v| *v >= 0) else {
                steps.push(json!({ "outcome": "failed", "message": "连登抽奖次数未知，未消费" }));
                return Ok(summary(account, steps));
            };
            let count = count.min(20) as usize;
            for _ in 0..count {
                match idempotent(
                    account,
                    "streak-draw-pending",
                    "/activity/growth/lottery/draw",
                    &json!({}),
                ) {
                    Ok(prize) => {
                        let stop = prize["stopAfterKnownResult"] == true;
                        steps.push(
                            json!({ "outcome": "draw", "message": "抽奖完成", "prize": prize }),
                        );
                        if stop {
                            break;
                        }
                    }
                    Err(error) => {
                        steps.push(json!({ "outcome": "failed", "message": error }));
                        break;
                    }
                }
                if let Err(error) = account.pause(super::GROWTH_WRITE_GAP_MS) {
                    steps.push(json!({ "outcome": "failed", "message": error }));
                    break;
                }
            }
        }
        Err(error) => steps.push(json!({ "outcome": "failed", "message": error })),
    }
    Ok(summary(account, steps))
}
fn summary(account: &GrowthAccount, steps: Vec<Value>) -> Value {
    let done = steps
        .iter()
        .filter(|v| matches!(v["outcome"].as_str(), Some("makeup" | "redeem" | "draw")))
        .count();
    let failed = steps.iter().filter(|v| v["outcome"] == "failed").count();
    let messages: Vec<_> = steps.iter().filter_map(|v| v["message"].as_str()).collect();
    json!({ "results": [{ "accountId": account.session.record["id"], "name": account.nick, "done": done, "failed": failed, "messages": messages }],
        "steps": steps, "done": done, "failed": failed })
}
