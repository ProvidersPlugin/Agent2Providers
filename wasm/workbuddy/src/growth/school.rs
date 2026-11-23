//! 开学季任务和抽奖独立活动空间；最多5条任务、每条12轮、20抽，学生认证不代办。
//! draw_uuid发出前记账，结果未知不更换一次性uuid重发，避免重复消费。
use super::{events, GrowthAccount, GrowthCall, MP_UA};
use crate::{endpoints, ledger, util};
use agent2api_wasm_sdk::{self as sdk, json, Value};

pub const ACTIVITY_ID: &str = "school_open_day_2026";
fn request(
    account: &GrowthAccount,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<GrowthCall, String> {
    super::bill_call(
        account,
        method,
        path,
        body,
        &[("User-Agent".into(), MP_UA.into())],
    )
}
pub fn tasks(account: &GrowthAccount) -> Result<(Vec<Value>, bool), String> {
    let reply = request(account, "GET", "/portal/activity/school/tasks", None)?;
    if !reply.ok() {
        return Err(format!("开学季任务查询失败：{}", reply.msg()));
    }
    let rows = reply.data()["tasks"]
        .as_array()
        .cloned()
        .ok_or("开学季任务响应缺少tasks")?;
    Ok((rows, reply.data()["in_period"] == true))
}
pub fn lottery(account: &GrowthAccount) -> Result<Value, String> {
    let reply = request(account, "GET", "/portal/activity/school/config", None)?;
    if !reply.ok() {
        return Err(format!("开学季转盘查询失败：{}", reply.msg()));
    }
    Ok(reply.data().clone())
}
fn post(account: &GrowthAccount, path: &str, body: Option<&Value>) -> Result<Value, String> {
    let reply = request(account, "POST", path, body)?;
    if !reply.ok() {
        return Err(format!("开学季操作失败：{}", reply.msg()));
    }
    Ok(reply.data().clone())
}
pub fn mini_chat_event(account: &GrowthAccount, conversation: Option<&str>) -> Value {
    let now = sdk::now();
    let id = conversation
        .map(str::to_string)
        .unwrap_or_else(|| format!("wbmp-{now}"));
    json!({ "eventCode": "chat_request_send", "timestamp": now, "reportDelay": 0, "source": "mini_program", "ideName": "wx_app_cloud", "ideType": "WorkBuddy_MP",
        "extName": "workbuddy-mp", "extVersion": "SaaS", "mode": "chat", "conversationId": id, "requestId": id, "inputLength": 12,
        "activityId": ACTIVITY_ID, "mentionContexts": [], "mentionContextCount": 0, "userId": account.uid })
}
fn school_expert(account: &GrowthAccount) -> (String, String) {
    let reply = request(
        account,
        "POST",
        "/v2/operation-platform/market/expert/list",
        Some(&json!({ "edition_mode": "all,domestic", "page": 1,
        "page_size": 20, "sort_by": "use_count", "sort_order": "desc", "categories": ["16-BackToSchool"], "expert_type": "agent" })),
    );
    if let Ok(reply) = reply {
        if reply.ok() {
            if let Some(row) = reply.data()["experts"]
                .as_array()
                .and_then(|rows| rows.first())
            {
                if let Some(id) = row["expert_id"].as_str().filter(|s| !s.is_empty()) {
                    let name = row["display_name_zh"]["zh"]
                        .as_str()
                        .or_else(|| row["display_name_zh"].as_str())
                        .unwrap_or(id);
                    return (id.into(), name.into());
                }
            }
        }
    }
    ("ex_jB0dyFIQJEWa".into(), "论小舟".into())
}
fn expert_event(account: &GrowthAccount) -> Value {
    let (id, name) = school_expert(account);
    let now = sdk::now();
    json!({ "eventCode": "expert_actual_use", "timestamp": now, "reportDelay": 0, "source": "mini_program", "ideName": "wx_app_cloud", "ideType": "WorkBuddy_MP",
        "extName": "workbuddy-mp", "extVersion": "SaaS", "machineId": account.derive_id("machine"), "os": "android", "osVersion": "14", "arch": "arm64", "timezone": "Asia/Shanghai",
        "userId": account.uid, "userNickname": account.nick, "id": id, "name": id, "expertTitle": name, "type": "send_message", "characterCount": 12,
        "expertType": "agent", "conversationId": format!("wbexp-{now}"), "activityId": ACTIVITY_ID })
}
fn report(account: &GrowthAccount, kind: &str) -> Result<(), String> {
    if kind == "desktop" {
        let id = format!("wbdesk-{}", sdk::now());
        let mut events = events::desktop_chat_sequence(account, &id, &id, &id);
        for event in &mut events {
            event["activityId"] = json!(ACTIVITY_ID);
        }
        return events::report_desktop_events(account, &events);
    }
    let event = if kind == "expert" {
        expert_event(account)
    } else {
        mini_chat_event(account, None)
    };
    post(account, "/v2/report", Some(&json!([event])))?;
    Ok(())
}
fn row(code: &str, outcome: &str, message: &str) -> Value {
    json!({ "code": code, "outcome": outcome, "message": message })
}
fn process(account: &GrowthAccount, task: &Value) -> Result<Value, String> {
    let code = util::text(task, &["task_code"]);
    let state = util::text(task, &["status"]).to_ascii_lowercase();
    if state == "claimed" {
        return Ok(row(&code, "already", "奖励已领取"));
    }
    let kind = match code.as_str() {
        "task_student_verify" => return Ok(row(&code, "skipped", "学生认证须本人完成")),
        "share_invite" => "share",
        "chat_3_times" => "chat",
        "desktop_chat_1_time" => "desktop",
        "expert_use" => "expert",
        _ => return Ok(row(&code, "skipped", "未确认任务，未执行")),
    };
    let mut current = util::integer(&task["progress"]).unwrap_or(0);
    let target = util::integer(&task["target_count"]).unwrap_or(1).max(1);
    let mut completed = state == "completed" || current >= target;
    let path = format!("/portal/activity/school/tasks/{}", endpoints::encode(&code));
    if !completed && state == "pending" {
        post(account, &format!("{path}/viewed"), None)?;
        account.pause(super::GROWTH_WRITE_GAP_MS)?;
    }
    for _ in 0..if kind == "desktop" || kind == "share" {
        1
    } else {
        target.saturating_sub(current).clamp(0, 12) as usize
    } {
        if completed {
            break;
        }
        if kind == "share" {
            post(
                account,
                "/portal/activity/school/tasks/share-complete",
                Some(&json!({ "channel": "wechat" })),
            )?;
        } else {
            report(account, kind)?;
        }
        account.pause(super::GROWTH_WRITE_GAP_MS)?;
        let (tasks, in_period) = tasks(account)?;
        if !in_period {
            return Ok(row(&code, "skipped", "活动已经结束"));
        }
        let after = super::find_task(&tasks, &code).ok_or("开学季任务回读缺失")?;
        if after["status"] == "claimed" {
            return Ok(row(&code, "already", "回读确认已领奖"));
        }
        current = util::integer(&after["progress"]).unwrap_or(current);
        completed = after["status"] == "completed" || current >= target;
    }
    if completed {
        post(account, &format!("{path}/claim"), None)?;
        Ok(row(&code, "done", "已完成任务并领取奖励"))
    } else {
        Ok(row(
            &code,
            "skipped",
            &format!("回读进度{current}/{target}，下次继续"),
        ))
    }
}
pub fn run(account: &GrowthAccount, payload: &Value) -> Result<Value, String> {
    let (tasks, active) = tasks(account)?;
    if !active {
        return Ok(
            json!({ "id": account.session.record["id"], "results": [row("(activity)", "skipped", "活动非进行期")], "lottery": null }),
        );
    }
    let only: Vec<_> = payload["only"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut reports = Vec::new();
    for task in tasks
        .iter()
        .filter(|task| {
            only.is_empty()
                || task["task_code"]
                    .as_str()
                    .is_some_and(|s| only.contains(&s))
        })
        .take(5)
    {
        if let Err(error) = account.check() {
            reports.push(row(
                task["task_code"].as_str().unwrap_or(""),
                "skipped",
                &error,
            ));
            break;
        }
        let result = process(account, task).unwrap_or_else(|error| {
            row(task["task_code"].as_str().unwrap_or(""), "failed", &error)
        });
        reports.push(result);
    }
    let lottery = if payload["lottery"] == true {
        draw_all(account).unwrap_or_else(|error| {
            json!({ "success": false, "drawn": 0, "credit": 0,
            "prizes": [], "message": error })
        })
    } else {
        Value::Null
    };
    Ok(json!({ "id": account.session.record["id"], "results": reports, "lottery": lottery }))
}
fn draw_all(account: &GrowthAccount) -> Result<Value, String> {
    let config = lottery(account)?;
    if config["in_period"] != true {
        return Ok(json!({ "success": false, "message": "活动非进行期，未抽奖" }));
    }
    let mut balance = util::integer(&config["chance"]["balance"])
        .filter(|v| *v >= 0)
        .ok_or("抽奖次数未知，未消费")?;
    let mut prizes = Vec::new();
    let mut credit: Option<i64> = Some(0);
    let mut drawn = 0;
    let mut remaining = Some(balance);
    let mut failure = None;
    for _ in 0..balance.clamp(0, 20) {
        let result = (|| -> Result<Option<Value>, String> {
            account.check()?;
            let old = ledger::get(&account.session, "school-draw").map_err(|e| e.message)?;
            if old["pending"] == true {
                return Err(
                    "上次开学季抽奖结果未确认，未换新uuid继续消费次数，请在官方页面核对".into(),
                );
            }
            let uuid = util::uuid().map_err(|e| e.message)?;
            ledger::put(
                &account.session,
                "school-draw",
                &json!({ "pending": true, "drawUuid": uuid, "at": sdk::now() }),
            )
            .map_err(|e| e.message)?;
            let reply = request(
                account,
                "POST",
                "/portal/activity/school/wheel/draw",
                Some(&json!({ "draw_uuid": uuid })),
            )?;
            if reply.code() == Some(40900) {
                ledger::put(
                    &account.session,
                    "school-draw",
                    &json!({ "pending": false, "message": "次数已用尽" }),
                )
                .map_err(|e| e.message)?;
                return Ok(None);
            }
            if !reply.ok() {
                return Err(format!("抽奖结果未确认：{}", reply.msg()));
            }
            Ok(Some(reply.data().clone()))
        })();
        let data = match result {
            Ok(Some(data)) => data,
            Ok(None) => {
                remaining = Some(0);
                break;
            }
            Err(error) => {
                remaining = None;
                failure = Some(error);
                break;
            }
        };
        drawn += 1;
        let amount = util::integer(&data["credit_amount"]);
        credit = credit.zip(amount).map(|(a, b)| a.saturating_add(b));
        let item = json!({ "code": data["prize_code"], "label": prize_label(data["prize_code"].as_str().unwrap_or("")), "credit": amount });
        prizes.push(item.clone());
        remaining = util::integer(&data["chance_balance"]).filter(|count| *count >= 0);
        if let Err(error) = ledger::put(
            &account.session,
            "school-draw",
            &json!({ "pending": false, "result": item, "at": sdk::now() }),
        ) {
            failure = Some(format!(
                "本次抽奖已返回，但消费台账未保存，已停止后续抽奖：{}",
                error.message
            ));
            break;
        }
        let Some(next) = remaining else { break };
        if next <= 0 || next >= balance {
            break;
        }
        balance = next;
        if let Err(error) = account.pause(super::GROWTH_WRITE_GAP_MS) {
            failure = Some(error);
            break;
        }
    }
    Ok(
        json!({ "success": failure.is_none(), "drawn": drawn, "credit": credit, "remaining": remaining,
        "prizes": prizes, "message": failure.unwrap_or_else(|| "本轮抽奖已结束".into()) }),
    )
}
fn prize_label(code: &str) -> &str {
    match code {
        "school_credit_6" => "6积分",
        "school_credit_66" => "66积分",
        "school_voucher_luckin" => "瑞幸咖啡15元券",
        "school_voucher_kfc_ok" => "肯德基OK餐券",
        "school_voucher_kfc_ice" => "肯德基冰淇淋券",
        "school_voucher_kugou" => "酷狗会员月卡券",
        other => other,
    }
}
