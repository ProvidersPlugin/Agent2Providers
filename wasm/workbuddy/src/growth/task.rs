//! 有限单任务accept→上报→回读→claim；同任务每轮最多12次，不凭上报200算已完成。
use super::{events, expert, find_task, list_tasks, spec, travel, GrowthAccount};
use crate::{endpoints, util};
use agent2api_wasm_sdk::{json, Value};

pub fn claim(account: &GrowthAccount, code: &str, mp: bool) -> Result<Value, String> {
    let path = format!("/activity/growth/tasks/{}/claim", endpoints::encode(code));
    let reply = super::chat_call(
        account,
        "POST",
        &path,
        None,
        &events::mp_platform_header(mp),
    )?;
    let reply = if reply.status == 400 {
        super::web_call(
            account,
            "POST",
            &path,
            None,
            &events::web_claim_headers(account),
        )?
    } else {
        reply
    };
    if !reply.ok() {
        return Err(format!(
            "成长领奖失败（HTTP {}）：{}",
            reply.status,
            reply.msg()
        ));
    }
    let accepted =
        find_task(&list_tasks(account, mp)?, code).ok_or("领奖后任务状态未确认，未报告领取成功")?;
    if util::text(&accepted, &["accept_status"]) != "claimed" {
        return Err("领奖响应已返回，但回读未确认已领取；请在官方页面核对，未自动重领".into());
    }
    let already = reply.data()["already_claimed"] == true;
    Ok(
        json!({ "outcome": if already { "already" } else { "done" }, "message": if already { "奖励已领取" } else { "奖励领取成功" },
        "credit": if already { Some(0) } else { util::integer(&reply.data()["credit"]) }, "energy": if already { Some(0) } else { util::integer(&reply.data()["energy"]) } }),
    )
}
fn accept(account: &GrowthAccount, code: &str, mp: bool) -> Result<(), String> {
    for _ in 0..2 {
        let reply = super::chat_call(
            account,
            "POST",
            "/v2/activity/growth/tasks/accept",
            Some(&json!({ "task_codes": [code] })),
            &events::mp_platform_header(mp),
        )?;
        let accepted = reply.ok()
            && reply.data()["results"]
                .as_array()
                .and_then(|v| v.first())
                .is_some_and(|v| v["status"] == "accepted");
        let after = find_task(&list_tasks(account, mp)?, code).unwrap_or(Value::Null);
        let status = util::text(&after, &["accept_status"]);
        if !status.is_empty()
            && status != "not_accepted"
            && (accepted
                || matches!(
                    status.as_str(),
                    "accepted" | "in_progress" | "completed" | "claimed"
                ))
        {
            return Ok(());
        }
        account.pause(super::GROWTH_WRITE_GAP_MS)?;
    }
    Err("任务接单未登记，本轮不继续上报".into())
}
fn skipped(message: &str) -> Value {
    json!({ "outcome": "skipped", "message": message, "credit": 0, "energy": 0 })
}
pub fn run(account: &GrowthAccount, payload: &Value) -> Result<Value, String> {
    let code = util::text(payload, &["code"]);
    if code.is_empty()
        || code.len() > 128
        || !code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err("成长任务码无效".into());
    }
    let mode = util::text(payload, &["mode"]);
    if !matches!(mode.as_str(), "" | "full" | "claim") {
        return Err("任务模式只能为full或claim".into());
    }
    let spec = spec::spec_of(&code);
    let mp = spec.as_ref().is_some_and(|s| s.mp);
    let rows = list_tasks(account, mp)?;
    let Some(task) = find_task(&rows, &code) else {
        return Ok(skipped("任务已下线或不存在"));
    };
    let mut result = process(account, &code, &mode, &task, spec.as_ref())?;
    result["id"] = account.session.record["id"].clone();
    result["code"] = json!(code);
    Ok(result)
}
fn process(
    account: &GrowthAccount,
    code: &str,
    mode: &str,
    task: &Value,
    spec: Option<&spec::TaskSpec>,
) -> Result<Value, String> {
    let state = util::text(task, &["accept_status"]);
    let current = util::integer(&task["progress"]["current"]).unwrap_or(0);
    let target = util::integer(&task["progress"]["target"])
        .or_else(|| spec.map(|s| s.target))
        .unwrap_or(1);
    if state == "claimed" {
        return Ok(
            json!({ "outcome": "already", "message": "奖励已领取", "credit": 0, "energy": 0 }),
        );
    }
    if state == "completed" || target > 0 && current >= target {
        return claim(account, code, spec.is_some_and(|s| s.mp));
    }
    if mode == "claim" {
        return Ok(skipped("尚未完成，未上报也未领取"));
    }
    let Some(spec) = spec else {
        return Ok(skipped("未确认的新任务，保守跳过"));
    };
    if spec.unforgeable {
        return Ok(skipped(&format!("需本人操作：{}", spec.reason)));
    }
    if task["locked"] == true {
        return Ok(skipped("任务尚未解锁，未继续操作"));
    }
    if code == "black_cat" && !events::within_night_window() {
        return Ok(skipped("非北京时间23:00至08:00夜猫窗口"));
    }
    if state.is_empty() || state == "not_accepted" {
        accept(account, code, spec.mp)?;
    }
    if code == "first_buddy" {
        let mut adopted = travel::adopt(account)?;
        if adopted["outcome"] != "adopt" {
            return Ok(adopted);
        }
        let reward = (|| {
            account.pause(super::GROWTH_WRITE_GAP_MS)?;
            let task = find_task(&list_tasks(account, false)?, code)
                .ok_or("领养已成功，但任务回读缺失")?;
            let status = util::text(&task, &["accept_status"]);
            let current = util::integer(&task["progress"]["current"]);
            let target = util::integer(&task["progress"]["target"]);
            if status == "claimed" {
                return Ok(json!({ "outcome": "already", "credit": 0, "energy": 0 }));
            }
            if status != "completed"
                && !current
                    .zip(target)
                    .is_some_and(|(current, target)| target > 0 && current >= target)
            {
                return Err("领养已成功，任务尚未结算为可领取".to_string());
            }
            claim(account, code, false)
        })();
        match reward {
            Ok(reward) => {
                if reward["outcome"] == "done" {
                    for key in ["credit", "energy"] {
                        adopted[key] = json!(util::integer(&adopted[key])
                            .zip(util::integer(&reward[key]))
                            .map(|(a, b)| a.saturating_add(b)));
                    }
                }
                adopted["outcome"] = json!("done");
                adopted["taskReward"] = reward;
            }
            Err(error) => {
                adopted["outcome"] = json!("done");
                adopted["warning"] = json!(format!("领养成功，任务结算待补领：{error}"));
            }
        }
        return Ok(adopted);
    }
    let need = target
        .saturating_sub(current)
        .clamp(0, if code == "black_cat" { 1 } else { 12 }) as usize;
    let objects = if spec.kind == "expert-task" {
        vec![expert::resolve(account, task)?]
    } else {
        spec::ids_for(spec.kind, account, need, current.max(0) as usize)?
    };
    if need > 0 && spec.needs_object && objects.is_empty() {
        return Ok(skipped("没有可用的任务对象，本轮未上报"));
    }
    if spec.kind == "expert-task" {
        expert::use_expert(account, objects.first().ok_or("任务未提供指定专家")?)?;
    } else if let Some(error) = events::light_up(account, code, spec.kind, &objects, need) {
        return Err(error);
    }
    account.pause(1800)?;
    let after = find_task(&list_tasks(account, spec.mp)?, code).ok_or("回读时任务已不在列表")?;
    let status = util::text(&after, &["accept_status"]);
    let progress = util::integer(&after["progress"]["current"]).unwrap_or(0);
    let target = util::integer(&after["progress"]["target"]).unwrap_or(target);
    if status == "claimed" {
        return Ok(
            json!({ "outcome": "already", "message": "回读确认已领取", "credit": 0, "energy": 0 }),
        );
    }
    if status == "completed" || target > 0 && progress >= target {
        return claim(account, code, spec.mp);
    }
    Ok(skipped(&format!(
        "回读进度{progress}/{target}，未确认完成；下次从官方状态继续"
    )))
}
