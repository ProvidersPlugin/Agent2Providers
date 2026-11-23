//! 仅查询活动配置和官方任务状态，不包含 landing/checkin 或 reward/claim 端点。
//! 任务状态和奖励未知时保留 null；每日任务只认 checked_in_today，不能用 status=3 覆盖 false。

use std::collections::BTreeMap;

use agent2api_wasm_sdk::{json, Result, Value};
use serde::Serialize;

use crate::{
    account::{self, Snapshot},
    endpoints::ReadEndpoint,
    errors, util,
};

const V3_GROUP: &str = "pc_incentive_task_mvp_v3";
const MAX_TASKS: usize = 512;

pub struct Activity {
    pub value: Value,
    pub summary: String,
}

#[derive(Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Task {
    key: String,
    group: String,
    title: String,
    status: Option<u64>,
    daily: bool,
    checked_in_today: Option<bool>,
    completed: Option<bool>,
    claimable: Option<bool>,
    reward_amount: Option<f64>,
    streak: Option<u64>,
    cycle: Option<String>,
    read_only: bool,
    claim_enabled: bool,
}

fn code(value: &Value, secrets: &[String]) -> Result<String> {
    let text = util::identifier(value)
        .filter(|value| !util::protected(value, secrets) && !value.contains("://"))
        .ok_or_else(|| errors::protocol("豆包工作活动标识无效"))?;
    Ok(text)
}

fn inherited_group(value: &Value, group: &str, secrets: &[String]) -> Result<String> {
    match value
        .get("campaign_group_code")
        .or_else(|| value.get("campaign_code"))
        .filter(|value| !value.is_null() && value.as_str() != Some(""))
    {
        Some(value) => code(value, secrets),
        None => Ok(group.into()),
    }
}

fn collect(
    value: &Value,
    group: &str,
    depth: usize,
    tasks: &mut BTreeMap<(String, String), Task>,
    recognized: &mut bool,
    scanned: &mut usize,
    secrets: &[String],
) -> Result<()> {
    if depth > 8 || !value.is_object() {
        return Err(errors::protocol("豆包工作活动结构无效或嵌套过深"));
    }
    let group = inherited_group(value, group, secrets)?;
    for key in ["tasks", "campaigns", "milestones"] {
        let Some(rows) = value.get(key) else {
            continue;
        };
        *recognized = true;
        let rows = rows
            .as_array()
            .filter(|rows| rows.len() <= MAX_TASKS)
            .ok_or_else(|| errors::protocol("豆包工作活动任务列表无效"))?;
        for row in rows {
            *scanned += 1;
            if *scanned > MAX_TASKS || !row.is_object() {
                return Err(errors::protocol("豆包工作活动任务过多或条目无效"));
            }
            let item = code(&row["campaign_code"], secrets)?;
            let task_group = match row
                .get("campaign_group_code")
                .filter(|value| !value.is_null())
            {
                Some(value) => code(value, secrets)?,
                None => group.clone(),
            };
            let status = match row.get("task_status").filter(|value| !value.is_null()) {
                Some(value) => Some(
                    util::unsigned(value)
                        .ok_or_else(|| errors::protocol("豆包工作任务状态编号无效"))?,
                ),
                None => None,
            };
            let daily_data = row.get("daily_checkin").filter(|value| !value.is_null());
            if daily_data.is_some_and(|value| !value.is_object()) {
                return Err(errors::protocol("豆包工作每日任务状态不是对象"));
            }
            let checked = daily_data
                .and_then(|value| value.get("checked_in_today"))
                .filter(|value| !value.is_null());
            if checked.is_some_and(|value| !value.is_boolean()) {
                return Err(errors::protocol("豆包工作每日任务完成标记无效"));
            }
            let daily = daily_data.is_some() || row["task_kind"] == "daily_signin";
            let checked = checked.and_then(Value::as_bool);
            let completed = if daily {
                checked
            } else {
                match status {
                    Some(2 | 3) => Some(true),
                    Some(1 | 4) => Some(false),
                    _ => None,
                }
            };
            let reward_amount = row
                .pointer("/daily_checkin/reward/reward_amount")
                .and_then(util::number)
                .or_else(|| {
                    row.pointer("/reward_summaries/0/reward_amount")
                        .and_then(util::number)
                });
            let title = util::safe_text(util::text(row, &["title", "name"]), secrets, 160);
            let task = Task {
                key: item.clone(),
                group: task_group.clone(),
                title: if title.is_empty() {
                    item.clone()
                } else {
                    title
                },
                status,
                daily,
                checked_in_today: checked,
                completed,
                claimable: if daily {
                    Some(false)
                } else {
                    status.map(|status| status == 2)
                },
                reward_amount,
                streak: util::unsigned(&row["daily_checkin"]["continuous_days"]),
                cycle: None,
                read_only: true,
                claim_enabled: false,
            };
            if let Some(previous) = tasks.get(&(task_group.clone(), item.clone())) {
                if previous != &task {
                    return Err(errors::protocol(
                        "豆包工作活动同一任务状态冲突，未任选一份结果",
                    ));
                }
            } else {
                tasks.insert((task_group, item), task);
            }
        }
    }
    if let Some(child) = value.get("campaign_group") {
        collect(
            child,
            &group,
            depth + 1,
            tasks,
            recognized,
            scanned,
            secrets,
        )?;
    }
    if let Some(children) = value.get("campaign_groups") {
        let children = children
            .as_array()
            .filter(|items| items.len() <= MAX_TASKS)
            .ok_or_else(|| errors::protocol("豆包工作活动分组格式无效"))?;
        if children.is_empty() {
            *recognized = true;
        }
        for child in children {
            collect(
                child,
                &group,
                depth + 1,
                tasks,
                recognized,
                scanned,
                secrets,
            )?;
        }
    }
    Ok(())
}

pub fn read(account: &Snapshot) -> Result<Activity> {
    let launch = match account.read(ReadEndpoint::BasicLaunch, &json!({})) {
        Ok(value) => value,
        Err(error) if matches!(error.status, Some(404 | 405)) => {
            account.read(ReadEndpoint::UserLaunch, &json!({}))?
        }
        Err(error) => return Err(error),
    };
    let launch = launch
        .get("data")
        .filter(|value| {
            value.is_object() && value.as_object().is_some_and(|fields| !fields.is_empty())
        })
        .ok_or_else(|| errors::protocol("豆包工作未返回有效活动配置，不能据此判断没有活动"))?;
    let info = launch.get("ug_campaign_info").ok_or_else(|| {
        errors::protocol("豆包工作未下发可识别活动字段，当前状态未知，未记为空活动")
    })?;
    // 明确的空活动字段才表示本次未下发；缺字段或坏结构不能变成成功空结果。
    if info.is_null() {
        return Ok(Activity {
            value: json!({ "state": "inactive", "tasks": [], "readOnly": true,
            "campaignGroup": null, "completed": null, "rewardAmount": null, "queriedAt": util::now()? }),
            summary: "当前账号未下发活动入口；本次仅查询，没有领取".into(),
        });
    }
    if !info.is_object() {
        return Err(errors::protocol("豆包工作活动配置格式无效"));
    }
    let group = code(&info["campaign_group_code"], &account.credentials.protected)?;
    let endpoint = if group == V3_GROUP {
        ReadEndpoint::CampaignV3
    } else {
        ReadEndpoint::CampaignList
    };
    let value = account.read(endpoint, &json!({ "campaign_group_code": group }))?;
    let data = value
        .get("data")
        .filter(|value| value.is_object())
        .ok_or_else(|| errors::protocol("豆包工作活动详情格式无效"))?;
    if group == V3_GROUP
        && data.get("campaign_code").is_some_and(|value| {
            !value.is_null() && value.as_str() != Some("") && value.as_str() != Some(group.as_str())
        })
    {
        return Err(errors::protocol("豆包工作返回的活动组与请求不匹配"));
    }
    let mut tasks = BTreeMap::new();
    let mut recognized = false;
    let mut scanned = 0;
    collect(
        data,
        &group,
        0,
        &mut tasks,
        &mut recognized,
        &mut scanned,
        &account.credentials.protected,
    )?;
    if !recognized {
        return Err(errors::protocol(
            "豆包工作活动详情缺少可识别任务结构，未记为空活动",
        ));
    }
    let tasks: Vec<_> = tasks.into_values().collect();
    let daily: Vec<_> = tasks.iter().filter(|task| task.daily).collect();
    let daily_text = if daily.len() == 1 {
        match daily[0].checked_in_today {
            Some(true) => "官方显示今日已签到",
            Some(false) => "官方显示今日尚未签到",
            None => "今日签到状态未知",
        }
    } else if daily.is_empty() {
        "当前活动没有每日签到项"
    } else {
        "返回多个每日任务，未合并签到结论"
    };
    let unknown = tasks.iter().filter(|task| task.completed.is_none()).count();
    let mut summary = format!(
        "{daily_text}；已查询 {} 项活动，仅查看，不领取",
        tasks.len()
    );
    if unknown > 0 {
        summary.push_str("；部分任务状态未知");
    }
    Ok(Activity {
        value: json!({ "state": "queried", "campaignGroup": group, "tasks": tasks,
        "readOnly": true, "claimEnabled": false, "queriedAt": util::now()?,
        "cycle": null, "rewardTotal": null, "message": summary }),
        summary,
    })
}

pub fn query() -> Result<Value> {
    let account = account::current()?;
    account.verify()?;
    let result = read(&account)?;
    account.unchanged()?;
    Ok(result.value)
}
