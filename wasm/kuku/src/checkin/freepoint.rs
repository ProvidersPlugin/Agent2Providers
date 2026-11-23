//! 已完成的旧每日任务使用 rewardClaim 真正领取；taskComplete 只上报真实进度。
//! 查询不领取、未完成 CHAT 不伪报；未知写结果持久留待确认，同一期不自动重放。

use super::{binding, day, report};
use crate::{credentials::Credentials, endpoints, errors, network, session, util};
use agent2api_wasm_sdk::{
    json,
    state::{self, Scope},
    Error, Result, Value,
};

const KEY: &str = "kuku.freepoint.operations";
const TASKS: [&str; 2] = ["LOGIN", "CHAT"];
#[derive(Clone)]
struct Task {
    kind: String,
    activity: String,
    period: String,
    key: String,
    status: String,
    claimable: Option<f64>,
}
impl Task {
    fn operation(&self) -> String {
        util::hash(&format!("{}\0{}\0{}", self.activity, self.period, self.key))
    }
}
fn panel(c: &Credentials) -> Result<Value> {
    let value = network::get(
        &format!(
            "{}{}?{}",
            endpoints::BASE,
            endpoints::FREE_HOME,
            endpoints::WEB_QUERY
        ),
        network::headers(c),
        &c.secrets(),
    )?;
    errors::errno(&value, &c.secrets())?;
    session::unchanged(c)?;
    Ok(value)
}
fn tasks(value: &Value) -> Result<Vec<Task>> {
    let activities = value["data"]["activities"]
        .as_array()
        .filter(|rows| rows.len() <= 64)
        .ok_or_else(|| Error::http(502, "库库任务面板缺少有效活动清单"))?;
    let mut out = Vec::new();
    for activity in activities {
        let tabs = activity["tabs"]
            .as_array()
            .filter(|rows| rows.len() <= 64)
            .ok_or_else(|| Error::http(502, "库库任务活动缺少有效分组"))?;
        for tab in tabs {
            for task in tab["tasks"]
                .as_array()
                .filter(|rows| rows.len() <= 256)
                .ok_or_else(|| Error::http(502, "库库任务分组缺少有效任务"))?
            {
                let kind = util::text(task, &["task_type"]);
                if !TASKS.contains(&kind.as_str()) {
                    continue;
                }
                let activity_key = util::text(activity, &["activity_key"]);
                let period = activity["period_no"]
                    .as_str()
                    .map(str::trim)
                    .map(str::to_string)
                    .or_else(|| {
                        activity["period_no"]
                            .as_u64()
                            .map(|number| number.to_string())
                    })
                    .unwrap_or_default();
                let key = util::text(task, &["task_key"]);
                if !util::identifier(&activity_key, 256)
                    || !util::identifier(&key, 256)
                    || period.is_empty()
                    || period.len() > 20
                    || !period.bytes().all(|byte| byte.is_ascii_digit())
                {
                    return Err(Error::http(502, "库库任务领取标识无效，未发起领取"));
                }
                let next = Task {
                    kind,
                    activity: activity_key,
                    period,
                    key,
                    status: util::text(task, &["task_status"]),
                    claimable: util::number(task.get("claimable_point")),
                };
                if out
                    .iter()
                    .any(|old: &Task| old.operation() == next.operation())
                {
                    return Err(Error::http(502, "库库任务面板含重复领取标识"));
                }
                if out.len() >= 128 {
                    return Err(Error::http(502, "库库任务数量超过上限"));
                }
                out.push(next);
            }
        }
    }
    Ok(out)
}
fn fence(c: &Credentials, task: &Task, action: &str) -> Result<bool> {
    session::unchanged(c)?;
    let before = state::get(Scope::Account, KEY, 1)?;
    if !before.compatible {
        return Err(Error::http(409, "库库领取记录版本不兼容，未覆盖旧记录"));
    }
    let owner = binding(c);
    let mut value = if before.value["binding"] == owner || !before.value.is_null() {
        if before.value["binding"] != owner {
            return Err(Error::http(409, "库库领奖身份已变化，未清除旧待确认记录"));
        }
        before.value.clone()
    } else {
        json!({"binding":owner,"operations":{}})
    };
    let id = format!("{action}:{}", task.operation());
    if value["operations"].get(&id).is_some() {
        return Ok(false);
    }
    let operations = value["operations"]
        .as_object_mut()
        .ok_or_else(|| Error::http(409, "库库领取记录格式无效"))?;
    if operations.len() >= 256 {
        let oldest = operations
            .iter()
            .filter(|(_, value)| value["pending"] != true)
            .min_by_key(|(_, value)| value["at"].as_i64().unwrap_or(0))
            .map(|(key, _)| key.clone());
        if let Some(key) = oldest {
            operations.remove(&key);
        } else {
            return Err(Error::http(
                409,
                "库库待确认领取操作数量超过上限，未重放旧操作",
            ));
        }
    }
    operations.insert(id, json!({"pending":true,"at":util::now()?}));
    if !state::compare_exchange(Scope::Account, KEY, 1, before.revision, value, 0)? {
        return Err(Error::http(409, "库库领取记录已变化，未重复提交"));
    }
    Ok(true)
}
fn result_state(c: &Credentials, task: &Task, action: &str, response: &Value) -> Result<()> {
    session::unchanged(c)?;
    let before = state::get(Scope::Account, KEY, 1)?;
    if !before.compatible || before.value["binding"] != binding(c) {
        return Err(Error::http(409, "库库领取记录归属已变化"));
    }
    let id = format!("{action}:{}", task.operation());
    let mut value = before.value;
    value["operations"][&id] = response.clone();
    value["operations"][&id]["at"] = json!(util::now()?);
    if !state::compare_exchange(Scope::Account, KEY, 1, before.revision, value, 0)? {
        return Err(Error::http(409, "库库领取结果记录已变化，未重复提交"));
    }
    Ok(())
}
pub fn claim_locked() -> Result<Value> {
    let c = session::business_locked()?;
    let home = panel(&c)?;
    let mut rows = tasks(&home)?;
    if rows.is_empty() {
        return Ok(report("inactive", "库库当前没有可参加的旧每日任务"));
    }
    let chat = state::get(Scope::Account, "kuku.chat.completed", 1)?;
    let completed_chat =
        chat.compatible && chat.value["day"] == day()? && chat.value["binding"] == binding(&c);
    let mut messages = Vec::new();
    let mut wrote_progress = false;
    let mut reported_kinds = Vec::new();
    for task in rows
        .iter()
        .filter(|task| task.claimable == Some(0.0) && task.status != "FINISHED")
    {
        if reported_kinds.contains(&task.kind.as_str()) {
            continue;
        }
        reported_kinds.push(task.kind.as_str());
        if task.kind == "CHAT" && !completed_chat {
            messages.push("对话任务尚未有本日真实完成回执，未伪报完成或自动发送对话".to_string());
            continue;
        }
        if !fence(&c, task, "progress")? {
            messages.push("旧任务上报结果待核实，未再次提交".to_string());
            continue;
        }
        let value = network::form(
            &format!(
                "{}{}?{}",
                endpoints::BASE,
                endpoints::FREE_TASK,
                endpoints::WEB_QUERY
            ),
            network::headers(&c),
            &format!("task_type={}", util::query(&task.kind)),
            &c.secrets(),
        );
        let value = match value {
            Ok(value) => value,
            Err(error) if errors::host_stop(&error) || error.status == Some(401) => {
                return Err(error)
            }
            Err(error) => {
                messages.push(errors::safe_message(&error.message, &c.secrets()));
                continue;
            }
        };
        errors::errno(&value, &c.secrets())?;
        let status = value["data"]["complete_status"].as_str().unwrap_or("");
        result_state(
            &c,
            task,
            "progress",
            &json!({"pending":status != "SUCCESS" && status != "BADGE_ALREADY_RECEIVED","accepted":status == "SUCCESS" || status == "BADGE_ALREADY_RECEIVED"}),
        )?;
        wrote_progress = true;
    }
    if wrote_progress {
        rows = tasks(&panel(&c)?)?;
    }
    let mut claimed = Vec::new();
    let mut credit: Option<f64> = Some(0.0);
    for task in rows
        .iter()
        .filter(|task| task.claimable.is_some_and(|points| points > 0.0))
    {
        if !fence(&c, task, "reward")? {
            messages.push("本期奖励曾提交领取，结果待核实，未重复领取".to_string());
            continue;
        }
        let body = format!(
            "activity_key={}&period_id={}&task_key={}",
            util::query(&task.activity),
            util::query(&task.period),
            util::query(&task.key)
        );
        let response = network::form(
            &format!(
                "{}{}?{}",
                endpoints::BASE,
                endpoints::FREE_REWARD,
                endpoints::WEB_QUERY
            ),
            network::headers(&c),
            &body,
            &c.secrets(),
        );
        let value = match response {
            Ok(value) => value,
            Err(error) if errors::host_stop(&error) || error.status == Some(401) => {
                return Err(error)
            }
            Err(error) => {
                messages.push(errors::safe_message(&error.message, &c.secrets()));
                continue;
            }
        };
        errors::errno(&value, &c.secrets())?;
        if value["data"]["claim_status"] != "SUCCESS" {
            messages.push("库库未确认奖励领取结果，未重领".to_string());
            continue;
        }
        let amount = util::number(value["data"].get("claimed_point"));
        result_state(
            &c,
            task,
            "reward",
            &json!({"confirmed":true,"amount":amount}),
        )?;
        credit = credit
            .zip(amount)
            .map(|(total, points)| total + points)
            .filter(|total| total.is_finite());
        claimed.push(task.kind.as_str());
    }
    let latest = tasks(&panel(&c)?)?;
    let settled = !latest.is_empty() && latest
        .iter()
        .all(|task| task.status == "FINISHED" && task.claimable == Some(0.0));
    let outcome = if !claimed.is_empty() {
        "claimed"
    } else if settled {
        "done"
    } else {
        "failed"
    };
    if !claimed.is_empty() {
        messages.push(format!("库库确认已领取：{}", claimed.join("、")));
    } else if settled {
        messages.push("库库面板确认本期旧每日任务已完成且无待领奖励".into());
    } else if messages.is_empty() {
        messages.push("库库旧每日任务尚有未完成或未知状态，未当作奖励到账".into());
    }
    let mut result = report(outcome, &messages.join("；"));
    if !claimed.is_empty() {
        result["credit"] = json!(credit);
    }
    result["pendingConfirmation"] = json!(!settled);
    Ok(result)
}
