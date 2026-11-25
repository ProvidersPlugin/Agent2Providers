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
    // 明确的空活动字段（含空字符串 campaign_group_code，如飞书/企业账号）表示本次未下发活动入口。
    let group_value = info.get("campaign_group_code");
    if info.is_null()
        || (info.is_object()
            && group_value.is_none_or(|value| {
                value.is_null() || value.as_str().is_some_and(|text| text.trim().is_empty())
            }))
    {
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
        let task = daily[0];
        let streak_suffix = task
            .streak
            .filter(|days| *days > 0)
            .map(|days| format!("（已连续 {days} 天）"))
            .unwrap_or_default();
        match task.checked_in_today {
            Some(true) => format!("官方显示今日已签到{streak_suffix}"),
            Some(false) => {
                format!("官方显示今日尚未签到{streak_suffix}，请打开豆包工作桌面端手动签到")
            }
            None => "今日签到状态未知".to_string(),
        }
    } else if daily.is_empty() {
        "当前活动没有每日签到项".to_string()
    } else {
        "返回多个每日任务，未合并签到结论".to_string()
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

/// 只读查询今日签到状态，绝不提交 `/alice/ug/campaign/landing/checkin`。
pub fn checkin_readonly() -> Result<Value> {
    let account = account::current()?;
    account.verify()?;
    let result = read(&account)?;
    account.unchanged()?;
    if result.value["state"].as_str() == Some("inactive") {
        return Ok(json!({
            "outcome": "inactive",
            "message": "当前账号未开放豆包工作每日签到活动（仅只读查询）",
            "credit": null,
            "streak": null,
        }));
    }
    let tasks = result.value["tasks"].as_array();
    let daily: Vec<&Value> = tasks
        .into_iter()
        .flatten()
        .filter(|task| task["daily"].as_bool() == Some(true))
        .collect();
    if daily.is_empty() {
        return Ok(json!({
            "outcome": "inactive",
            "message": "当前活动没有每日签到项（仅只读查询）",
            "credit": null,
            "streak": null,
        }));
    }
    if daily.len() > 1 {
        return Ok(json!({
            "outcome": "failed",
            "message": "官方返回多个每日签到项，未合并签到结论（仅只读查询）",
            "credit": null,
            "streak": null,
        }));
    }
    let task = daily[0];
    let streak = task["streak"].as_u64();
    match task["checkedInToday"].as_bool() {
        Some(true) => Ok(json!({
            "outcome": "done",
            "message": "官方显示今日已完成签到（仅只读查询）",
            "credit": null,
            "streak": streak,
        })),
        Some(false) => Ok(json!({
            "outcome": "ineligible",
            "message": "官方显示今日尚未签到；豆包工作每日签到已锁定官方桌面端安全校验，请打开豆包工作桌面端手动签到（本页仅查询状态）",
            "credit": null,
            "streak": streak,
        })),
        None => Ok(json!({
            "outcome": "failed",
            "message": "官方未返回今日签到完成标记（仅只读查询）",
            "credit": null,
            "streak": streak,
        })),
    }
}
