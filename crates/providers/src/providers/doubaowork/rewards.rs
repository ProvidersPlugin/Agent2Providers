use std::collections::BTreeSet;
use std::sync::OnceLock;

fn claim_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::api;
use crate::errors::GatewayError;

const V3_GROUP: &str = "pc_incentive_task_mvp_v3";

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value[key]
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

async fn detail(
    client: &reqwest::Client,
    credentials: &Value,
) -> Result<Option<(String, Value)>, GatewayError> {
    let launch = api::post(client, credentials, "/alice/user/launch", &json!({})).await?;
    let Some(group) = text(&launch["data"]["ug_campaign_info"], "campaign_group_code") else {
        return Ok(None);
    };
    let path = if group == V3_GROUP {
        "/alice/ug/campaign/v3/detail/"
    } else {
        "/alice/ug/campaign/list/"
    };
    let value = api::post(
        client,
        credentials,
        path,
        &json!({"campaign_group_code":group}),
    )
    .await?;
    let data = value["data"]
        .as_object()
        .ok_or_else(|| GatewayError::with_status(502, "豆包工作活动详情格式无效"))?;
    if group == V3_GROUP
        && data
            .get("campaign_code")
            .and_then(Value::as_str)
            .is_some_and(|code| !code.is_empty() && code != group)
    {
        return Err(GatewayError::with_status(
            502,
            "豆包工作返回了不匹配的活动组",
        ));
    }
    Ok(Some((group.into(), value)))
}

#[derive(Clone, PartialEq)]
struct Task {
    key: String,
    title: String,
    group: String,
    state: u64,
    daily: bool,
    checked: bool,
    body: Option<Value>,
    points: Option<f64>,
    streak: Option<u32>,
}

fn tasks(value: &Value, group: &str) -> Vec<Task> {
    fn collect(value: &Value, group: &str, output: &mut Vec<Task>, depth: usize) {
        if depth > 8 {
            return;
        }
        let group = text(value, "campaign_group_code")
            .or_else(|| text(value, "campaign_code"))
            .unwrap_or(group);
        for key in ["tasks", "campaigns", "milestones"] {
            for row in value[key].as_array().into_iter().flatten() {
                let Some(item) = text(row, "campaign_code") else {
                    continue;
                };
                let group = text(row, "campaign_group_code").unwrap_or(group);
                let daily = row["daily_checkin"]["checked_in_today"].is_boolean();
                let checked = row["daily_checkin"]["checked_in_today"] == true;
                let state = api::number(&row["task_status"]).unwrap_or(0);
                let direct = api::number(&row["reward_mode"]) == Some(2) || key == "milestones";
                let body = if direct {
                    (!group.is_empty()).then(||json!({"task_id":"","campaign_group_code":group,"campaign_item_code":item}))
                } else {
                    text(row, "task_id").map(|id| json!({"task_id":id}))
                };
                let points =
                    crate::quota::number(row.pointer("/daily_checkin/reward/reward_amount"))
                        .or_else(|| {
                            crate::quota::number(row.pointer("/reward_summaries/0/reward_amount"))
                        });
                let task = Task {
                    key: item.into(),
                    title: text(row, "title")
                        .or_else(|| text(row, "name"))
                        .unwrap_or(item)
                        .into(),
                    group: group.into(),
                    state,
                    daily,
                    checked,
                    body: if state == 2 || (daily && direct && matches!(state, 1 | 4)) {
                        body
                    } else {
                        None
                    },
                    points,
                    streak: api::number(&row["daily_checkin"]["continuous_days"])
                        .and_then(|days| u32::try_from(days).ok()),
                };
                if !output
                    .iter()
                    .any(|old| old.key == task.key && old.group == task.group)
                {
                    output.push(task);
                }
            }
        }
        if let Some(child) = value.get("campaign_group") {
            collect(child, group, output, depth + 1);
        }
        for child in value["campaign_groups"].as_array().into_iter().flatten() {
            collect(child, group, output, depth + 1);
        }
    }
    let mut output = Vec::new();
    collect(value.get("data").unwrap_or(value), group, &mut output, 0);
    output
}

fn receipt(value: &Value) -> Result<bool, GatewayError> {
    let data = value["data"]
        .as_object()
        .ok_or_else(|| GatewayError::with_status(502, "豆包工作未返回奖励领取回执"))?;
    if data.get("success") == Some(&Value::Bool(false)) {
        return Err(GatewayError::with_status(502, "豆包工作奖励领取被拒绝"));
    }
    if let Some(error) = data.get("biz_error").filter(|error| !error.is_null()) {
        if !matches!(api::number(&error["code"]), Some(0 | 4 | 6)) {
            return Err(api::error(200, error));
        }
    }
    Ok(api::number(&value["data"]["task_status"]) == Some(3)
        || value["data"]["checked_in_today"] == true
        || value["data"]["daily_checkin"]["checked_in_today"] == true)
}

pub async fn checkin(ctx: &PluginCtx, account: &str) -> Result<proto::CheckinReport, GatewayError> {
    let _guard = claim_lock().lock().await;
    let (_, credentials, client) = api::account(ctx, account, false).await?;
    let Some((group, value)) = detail(&client, &credentials).await? else {
        return Ok(report(
            proto::CheckinOutcome::Inactive,
            "当前没有豆包工作签到活动",
        ));
    };
    let daily: Vec<_> = tasks(&value, &group)
        .into_iter()
        .filter(|task| task.daily)
        .collect();
    if daily.len() != 1 {
        return Ok(report(
            proto::CheckinOutcome::Inactive,
            "当前活动未提供可确认的每日签到任务",
        ));
    }
    let Some(task) = daily.first() else {
        return Ok(report(
            proto::CheckinOutcome::Inactive,
            "当前没有每日签到任务",
        ));
    };
    if task.checked || task.state == 3 {
        return Ok(report(
            proto::CheckinOutcome::Done,
            "今日已领取豆包工作签到奖励",
        ));
    }
    let Some(body) = task.body.as_ref() else {
        return Ok(report(
            proto::CheckinOutcome::Inactive,
            "今日签到任务尚未满足领取条件",
        ));
    };
    let response = api::post(
        &client,
        &credentials,
        "/alice/ug/campaign/item/reward/claim/",
        body,
    )
    .await?;
    let confirmed = receipt(&response)?;
    let fresh = detail(&client, &credentials).await?;
    let after = fresh.as_ref().and_then(|(group, value)| {
        tasks(value, group)
            .into_iter()
            .find(|row| row.daily && row.key == task.key && row.group == task.group)
    });
    if !after
        .as_ref()
        .is_some_and(|task| task.checked || task.state == 3)
    {
        if confirmed {
            return Err(GatewayError::with_status(
                502,
                "豆包工作已接受领取，但回查尚未确认今日签到；稍后重试不会重复发奖",
            ));
        }
        return Err(GatewayError::with_status(
            502,
            "豆包工作未确认今日签到奖励，请稍后重试或前往官方活动页",
        ));
    }
    let points = crate::quota::number(response.pointer("/data/reward_amount")).or(task.points);
    Ok(proto::CheckinReport {
        outcome: proto::CheckinOutcome::Claimed,
        credit: points.map(|points| format!("{points} 额度")),
        streak: after.and_then(|task| task.streak).or(task.streak),
        message: Some("豆包工作签到奖励已领取，已回查确认".into()),
    })
}

fn report(outcome: proto::CheckinOutcome, message: &str) -> proto::CheckinReport {
    proto::CheckinReport {
        outcome,
        credit: None,
        streak: None,
        message: Some(message.into()),
    }
}

fn status_of(group: &str, value: &Value) -> Value {
    let tasks: Vec<_> = tasks(value, group)
        .into_iter()
        .filter(|task| !task.daily)
        .collect();
    let earned: f64 = tasks
        .iter()
        .filter(|task| task.state == 3)
        .filter_map(|task| task.points)
        .sum();
    let total: f64 = tasks.iter().filter_map(|task| task.points).sum();
    let unclaimed = tasks
        .iter()
        .filter(|task| task.body.is_some() && task.state != 3)
        .count();
    let rows: Vec<_> = tasks
        .iter()
        .map(|task| {
            json!({"key":task.key,"title":task.title,"group":task.group,"points":task.points,
        "done":task.state == 3,"blocked":task.body.is_none() && task.state != 3})
        })
        .collect();
    json!({"tasks":rows,"earned":earned,"total":total,"unclaimed":unclaimed,"settled":!tasks.is_empty() && tasks.iter().all(|task|task.state == 3)})
}

pub async fn status(ctx: &PluginCtx, account: &str) -> Result<Value, GatewayError> {
    let (_, credentials, client) = api::account(ctx, account, false).await?;
    Ok(match detail(&client, &credentials).await? {
        Some((group, value)) => status_of(&group, &value),
        None => json!({"tasks":[],"earned":0,"total":0,"unclaimed":0,"settled":false}),
    })
}

pub async fn claim_extras(ctx: &PluginCtx, account: &str) -> Result<Value, GatewayError> {
    let _guard = claim_lock().lock().await;
    let (_, credentials, client) = api::account(ctx, account, false).await?;
    let Some((group, value)) = detail(&client, &credentials).await? else {
        return Ok(json!({"tasks":[],"claimed":0,"failed":0,"unclaimed":0,"settled":false}));
    };
    let pending: Vec<_> = tasks(&value, &group)
        .into_iter()
        .filter(|task| !task.daily && task.body.is_some() && task.state != 3)
        .collect();
    let mut attempted = BTreeSet::new();
    let mut results = Vec::new();
    for task in pending {
        if !attempted.insert((task.group.clone(), task.key.clone())) {
            continue;
        }
        let Some(body) = task.body.as_ref() else {
            continue;
        };
        let response = api::post(
            &client,
            &credentials,
            "/alice/ug/campaign/item/reward/claim/",
            body,
        )
        .await;
        let result = match response {
            Ok(response) => receipt(&response).map(|_| ()),
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => results.push(json!({"key":task.key,"ok":true})),
            Err(error) => results.push(json!({"key":task.key,"ok":false,"error":error.message})),
        }
    }
    let fresh = detail(&client, &credentials).await?;
    let mut state = fresh
        .as_ref()
        .map(|(group, value)| status_of(group, value))
        .unwrap_or_else(|| status_of(&group, &value));
    let verified: BTreeSet<_> = fresh
        .as_ref()
        .map(|(group, value)| {
            tasks(value, group)
                .into_iter()
                .filter(|task| task.state == 3)
                .map(|task| task.key)
                .collect()
        })
        .unwrap_or_default();
    for result in &mut results {
        if result["ok"] == true && !verified.contains(result["key"].as_str().unwrap_or("")) {
            result["ok"] = json!(false);
            result["error"] = json!("官方尚未确认奖励到账，请稍后查询");
        }
    }
    let claimed = results.iter().filter(|result| result["ok"] == true).count();
    state["claimed"] = json!(claimed);
    state["failed"] = json!(results.len() - claimed);
    state["results"] = json!(results);
    Ok(state)
}
