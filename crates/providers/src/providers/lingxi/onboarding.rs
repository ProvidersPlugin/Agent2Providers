//! 新手任务（onboarding）：清单读取与「可领就领」。
//!
//! ── 上游没有「上报完成」的口子 ──────────────────────────────
//! 灵犀的 `GET /api/public/v1/tasks` 里 `type == "one_time"` 的任务，
//! 状态只有三种：`incomplete`（动作还没做）、`claimable`（动作做过了，
//! 可以领）、`claimed`（已领）。`incomplete` 直接 claim 会被上游拒
//! （400 `InvalidArgument`）；状态只能由**真实动作**换来（体验专家模式 /
//! 建群 / 用客户端）—— 网关不替用户刷任务，只在 `claimable` 时把奖励领回来。
//!
//! 返回形状与 Loomy / 小浣熊的新手任务面板同形（`tasks` / `earned` / `total` /
//! `unclaimed` / `settled`），界面按 `done` 与 `blocked` 渲染：
//! `blocked` = 动作还没做，点了也没用；只有既未完成、又未阻塞的才是「待领」。

use serde_json::{json, Value};

use agent2api_provider_sdk::PluginCtx;

use super::account;
use super::billing;
use super::constants;
use super::http;

/// 新手任务在界面上归到哪一组。
const GROUP: &str = "初识灵犀";

/// 读一次上游任务清单（`data.tasks`）。
async fn fetch_tasks(client: &reqwest::Client, sid: &str) -> Result<Vec<Value>, String> {
    let payload = http::get_json(client, sid, constants::TASKS_PATH).await?;
    Ok(http::data_of(&payload)
        .get("tasks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// 挑出**可领的一次性任务**（`type == one_time && status == claimable`）。
///
/// 这份判据同时服务签到链的自动领取与新手任务面板的「一键领取」—— 两处
/// 各写一份迟早会漂移（上游加一档状态时只改了一边）。
pub(super) fn claimable_one_time(tasks: &[Value]) -> Vec<(String, i64)> {
    tasks
        .iter()
        .filter_map(|task| {
            let key = task.get("task_key").and_then(Value::as_str)?;
            let kind = task.get("type").and_then(Value::as_str).unwrap_or("");
            let status = task.get("status").and_then(Value::as_str).unwrap_or("");
            if kind != "one_time" || status != "claimable" {
                return None;
            }
            let reward = task.get("reward_amount").and_then(Value::as_i64).unwrap_or(0);
            Some((key.to_string(), reward))
        })
        .collect()
}

/// 任务清单 → 界面行 + 汇总。
///
/// `done`：已领；`blocked`：动作还没做（claim 必被拒，界面不给「待领」计数）。
fn snapshot(tasks: &[Value]) -> Value {
    let mut rows = Vec::new();
    let mut earned = 0_i64;
    let mut total = 0_i64;
    let mut unclaimed = 0_i64;
    for task in tasks {
        let kind = task.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "one_time" {
            continue;
        }
        let Some(key) = task.get("task_key").and_then(Value::as_str) else {
            continue;
        };
        let status = task.get("status").and_then(Value::as_str).unwrap_or("");
        let points = task.get("reward_amount").and_then(Value::as_i64).unwrap_or(0);
        let title = task
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(key);
        let done = status == "claimed";
        let blocked = !done && status != "claimable";
        total += points;
        if done {
            earned += points;
        } else if !blocked {
            unclaimed += 1;
        }
        rows.push(json!({
            "key": key,
            "title": title,
            "group": GROUP,
            "points": points,
            "done": done,
            "blocked": blocked,
            "status": status,
        }));
    }
    json!({
        "tasks": rows,
        "earned": earned,
        "total": total,
        "unclaimed": unclaimed,
        "settled": unclaimed == 0,
    })
}

/// 任务状态（`claim_status`）：始终实查上游 —— 这里没有「结算记忆」，
/// 因为一次读请求很便宜，而缓存会让用户刚做完动作却看不到「可领」。
pub async fn get_tasks(
    ctx: &PluginCtx,
    account_id: &str,
    _refresh: bool,
) -> Result<Value, String> {
    let sid = account::sid_for(ctx, account_id).await?;
    let client = http::build_client(None, &ctx.env.proxy);
    let tasks = fetch_tasks(&client, &sid).await?;
    Ok(snapshot(&tasks))
}

/// 一键领取（`claim`）。`only` 非空时只领那一条。
///
/// 逐条串行：并发打同一上游没有收益，还容易撞风控。单条失败不中断其余，
/// 逐行带原因返回；领取后**重读任务清单**，界面拿到的就是上游的真实状态。
pub async fn claim_all(
    ctx: &PluginCtx,
    account_id: &str,
    only: &str,
) -> Result<Value, String> {
    let sid = account::sid_for(ctx, account_id).await?;
    let client = http::build_client(None, &ctx.env.proxy);
    let tasks = fetch_tasks(&client, &sid).await?;
    let before = billing::balance_total(&client, &sid).await.unwrap_or(-1.0);

    let mut results = Vec::new();
    let mut claimed = 0_i64;
    let mut failed = 0_i64;
    let mut claimed_points = 0_i64;
    for (key, expected) in claimable_one_time(&tasks) {
        if !only.trim().is_empty() && only != key {
            continue;
        }
        let path = format!("{}/{}/claim", constants::TASKS_PATH, key);
        match http::post_json(&client, &sid, &path, &json!({}), &[]).await {
            Ok(payload) => {
                let got = http::data_of(&payload)
                    .get("reward_amount")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                claimed += 1;
                claimed_points += if got > 0 { got } else { expected };
                results.push(json!({ "key": key, "ok": true, "points": got.max(0) }));
            }
            Err(error) => {
                failed += 1;
                results.push(json!({ "key": key, "ok": false, "error": error }));
            }
        }
    }

    let after = billing::balance_total(&client, &sid).await.unwrap_or(before);
    let tasks = fetch_tasks(&client, &sid).await.unwrap_or(tasks);
    let mut payload = snapshot(&tasks);
    if let Some(object) = payload.as_object_mut() {
        object.insert("results".to_string(), Value::Array(results));
        object.insert("claimed".to_string(), json!(claimed));
        object.insert("failed".to_string(), json!(failed));
        object.insert("claimedPoints".to_string(), json!(claimed_points));
        if claimed_points > 0 && before >= 0.0 && after - before < claimed_points as f64 {
            object.insert(
                "message".to_string(),
                json!(format!(
                    "回执 +{claimed_points} 但余额未同步（{}→{}），稍后重查",
                    billing::trim_number(before),
                    billing::trim_number(after)
                )),
            );
        }
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-16 实测的任务清单形状：只有 `claimable` 的一次性任务该领。
    fn measured() -> Vec<Value> {
        vec![
            json!({"task_key":"daily_check_in","type":"daily","status":"incomplete","reward_amount":100}),
            json!({"task_key":"try_group_chat","type":"one_time","status":"claimable","reward_amount":400,"title":"体验一次群聊功能"}),
            json!({"task_key":"try_expert_mode","type":"one_time","status":"claimed","reward_amount":200,"title":"完整体验一次灵犀专家模式"}),
            json!({"task_key":"use_lingxi_client","type":"one_time","status":"incomplete","reward_amount":800,"title":"使用灵犀客户端"}),
        ]
    }

    #[test]
    fn only_claimable_one_time_tasks_are_picked() {
        assert_eq!(
            claimable_one_time(&measured()),
            vec![("try_group_chat".to_string(), 400)]
        );
    }

    #[test]
    fn nothing_claimable_means_no_write() {
        let tasks = vec![json!({
            "task_key":"try_group_chat","type":"one_time","status":"claimed","reward_amount":400
        })];
        assert!(claimable_one_time(&tasks).is_empty());
    }

    #[test]
    fn snapshot_separates_done_blocked_and_claimable() {
        let payload = snapshot(&measured());
        assert_eq!(payload["total"], 1400);
        assert_eq!(payload["earned"], 200);
        assert_eq!(payload["unclaimed"], 1);
        assert_eq!(payload["settled"], false);
        let rows = payload["tasks"].as_array().expect("tasks");
        assert_eq!(rows.len(), 3, "每日签到不属于新手任务");
        let group = rows.iter().find(|row| row["key"] == "try_group_chat").expect("group");
        assert_eq!(group["blocked"], false);
        assert_eq!(group["done"], false);
        let expert = rows.iter().find(|row| row["key"] == "try_expert_mode").expect("expert");
        assert_eq!(expert["done"], true);
        let client = rows.iter().find(|row| row["key"] == "use_lingxi_client").expect("client");
        assert_eq!(client["blocked"], true);
    }

    #[test]
    fn all_claimed_settles() {
        let tasks = vec![
            json!({"task_key":"a","type":"one_time","status":"claimed","reward_amount":200}),
            json!({"task_key":"b","type":"one_time","status":"claimed","reward_amount":400}),
        ];
        let payload = snapshot(&tasks);
        assert_eq!(payload["unclaimed"], 0);
        assert_eq!(payload["settled"], true);
        assert_eq!(payload["earned"], 600);
    }
}
