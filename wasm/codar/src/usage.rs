use agent2api_wasm_sdk::{self as sdk, json, Result, Value};

use crate::{account, checkin, endpoints, network, token, util};

fn reading(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| util::amount(&value[*key]))
}

pub struct Points {
    pub general: Option<f64>,
    pub agent: Option<f64>,
    pub rewrite: Option<f64>,
}

pub fn points(current: &account::Snapshot) -> Result<Points> {
    let user = &current.secret["username"];
    let primary = current.request(
        "POST",
        endpoints::ACCOUNT,
        endpoints::POINTS,
        Some(&json!({ "username": user, "token": 0 })),
        8000,
    );
    let mut points = match &primary {
        Ok(value) => {
            let data = network::data(value);
            Points {
                general: reading(data, &["token", "remaining_points", "points", "balance"]),
                agent: reading(
                    data,
                    &["agent_token", "agentToken", "agent_points", "agentPoints"],
                ),
                rewrite: reading(data, &["rewrite_token", "rewriteToken", "rewrite_points"]),
            }
        }
        Err(error)
            if error.status == Some(401)
                || matches!(
                    error.code.as_deref(),
                    Some("codar_account_changed" | "codar_identity_mismatch")
                ) =>
        {
            return Err(error.clone())
        }
        _ => Points {
            general: None,
            agent: None,
            rewrite: None,
        },
    };
    if points.agent.is_none() {
        match current.request(
            "POST",
            endpoints::ACCOUNT,
            endpoints::AGENT_POINTS,
            Some(&json!({ "username": user })),
            5000,
        ) {
            Ok(value) => {
                points.agent = reading(
                    network::data(&value),
                    &[
                        "agent_balance",
                        "balance",
                        "points",
                        "agent_token",
                        "agentToken",
                        "agent_points",
                        "agentPoints",
                        "remaining_points",
                    ],
                );
            }
            Err(error)
                if error.status == Some(401)
                    || matches!(
                        error.code.as_deref(),
                        Some("codar_account_changed" | "codar_identity_mismatch")
                    ) =>
            {
                return Err(error)
            }
            Err(_) => {}
        }
    }
    if points.general.is_none() && points.agent.is_none() {
        return Err(primary.err().unwrap_or_else(|| {
            agent2api_wasm_sdk::Error::http(502, "科搭积分接口没有返回可读余额")
        }));
    }
    Ok(points)
}

pub fn sources(report: &Value) -> Value {
    let rows = report["windows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|window| window["aside"] != true)
        .filter_map(|window| {
            util::amount(&window["balance"]).map(|amount| {
                json!({ "id": window["id"], "provider": endpoints::PROVIDER,
            "label": window["name"], "remaining": amount, "unit": "点", "unlimited": false })
            })
        })
        .collect::<Vec<_>>();
    json!(rows)
}

pub fn query() -> Result<Value> {
    token::ensure(false)?;
    let current = account::current()?;
    let points = points(&current)?;
    let mut windows = Vec::new();
    for (id, name, value) in [
        ("general", "通用点数", points.general),
        ("agent", "Agent 点数", points.agent),
        ("rewrite", "改写点数", points.rewrite),
    ] {
        if let Some(balance) = value {
            windows.push(
                json!({ "id": id, "name": name, "balance": balance, "remaining": balance,
            "display": util::display(balance), "unit": "点", "aside": id == "rewrite" }),
            );
        }
    }
    if let Ok(status) = checkin::Status::query(&current) {
        windows.push(json!({ "name": "每日签到", "aside": true, "display": if status.signed { "已领取" } else { "可领取" }, "resetsAt": status.until }));
    }
    if let Ok(reply) = current.request("GET", endpoints::API, endpoints::ACTIVITIES, None, 5000) {
        if let Some(activities) = network::data(&reply).as_array() {
            for activity in activities.iter().take(20) {
                let until = util::timestamp(&activity["free_until"]);
                if until.is_none_or(|until| until <= sdk::now())
                    || activity
                        .get("remaining_quota")
                        .is_some_and(|value| !value.is_null() && util::amount(value) == Some(0.0))
                {
                    continue;
                }
                let models = activity["model_ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(4)
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>();
                let label = if models.is_empty() {
                    "免费模型活动".into()
                } else {
                    format!("免费模型 · {}", models.join("、"))
                };
                let remaining = util::amount(&activity["remaining_quota"]);
                windows.push(json!({ "name": label, "aside": true, "resetsAt": until,
                    "display": remaining.map(util::display).unwrap_or_else(|| "活动有效中".into()) }));
            }
        }
    }
    current.unchanged()?;
    let mut report = json!({ "provider": endpoints::PROVIDER, "plan": "科搭点数账户", "windows": windows,
        "user": current.secret["username"], "unit": "点", "message": "通用、Agent 与改写点数分别显示，不把活动免费期当成现金余额。" });
    report["quotaSources"] = sources(&report);
    Ok(report)
}

pub fn ledger(payload: &Value) -> Result<Value> {
    token::ensure(false)?;
    let current = account::current()?;
    let page = payload["page"].as_u64().unwrap_or(1).clamp(1, 1000);
    let size = payload["pageSize"].as_u64().unwrap_or(20).clamp(1, 100);
    current.request(
        "POST",
        endpoints::ACCOUNT,
        "/v1/agent/ledger",
        Some(&json!({ "username": current.secret["username"], "page": page, "page_size": size })),
        8000,
    )
}

pub fn daily(payload: &Value) -> Result<Value> {
    let start = payload["start"].as_str().unwrap_or("");
    let end = payload["end"].as_str().unwrap_or("");
    let parsed = chrono::NaiveDate::parse_from_str(start, "%Y-%m-%d")
        .ok()
        .zip(chrono::NaiveDate::parse_from_str(end, "%Y-%m-%d").ok());
    if parsed.is_none_or(|(start, end)| end < start || (end - start).num_days() >= 31) {
        return Err(agent2api_wasm_sdk::Error::http(
            400,
            "科搭用量查询需要 YYYY-MM-DD 格式的日期，范围不超过 31 天",
        ));
    }
    token::ensure(false)?;
    account::current()?.request(
        "GET",
        endpoints::API,
        &format!("/v1/codar/usage/daily?start_date={start}&end_date={end}"),
        None,
        8000,
    )
}
