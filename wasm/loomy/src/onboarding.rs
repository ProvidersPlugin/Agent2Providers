//! 原生八项新手任务：固定 key/积分，领取接口按 key 幂等，无额外行为证明。
//! 沿用账号 onboarding 台账的结算快照；读取/修改始终由宿主限定当前账号并加锁。
//! 只接手动 claim，不放入每日签到。全部已结算时零上游，refresh=true 可实查覆盖。

use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    AccountLock, Error, Result, Value,
};

use super::{account, client, errors};

struct Task {
    group: &'static str,
    key: &'static str,
    title: &'static str,
    points: i64,
}
const TASKS: [Task; 8] = [
    Task {
        group: "初识 Loomy",
        key: "first_message",
        title: "发送你的第一条消息",
        points: 500,
    },
    Task {
        group: "初识 Loomy",
        key: "pick_skill",
        title: "试试选择一个技能",
        points: 1000,
    },
    Task {
        group: "初识 Loomy",
        key: "generate_ppt",
        title: "生成第一份 PPT",
        points: 1500,
    },
    Task {
        group: "打造你的专属 Loomy",
        key: "set_schedule",
        title: "设置定时任务",
        points: 1000,
    },
    Task {
        group: "打造你的专属 Loomy",
        key: "install_skill",
        title: "在技能广场安装一个技能",
        points: 1500,
    },
    Task {
        group: "打造你的专属 Loomy",
        key: "configure_remote",
        title: "配置远程控制",
        points: 1000,
    },
    Task {
        group: "遇见你的 AI 搭子",
        key: "create_soul",
        title: "创建你的第一个搭子",
        points: 1500,
    },
    Task {
        group: "遇见你的 AI 搭子",
        key: "share_soul",
        title: "把搭子分享给朋友",
        points: 2000,
    },
];
const TOTAL: i64 = 10_000;

fn rows(done: &[bool]) -> Vec<Value> {
    TASKS
        .iter()
        .zip(done)
        .map(|(task, done)| {
            json!({ "key": task.key, "title": task.title,
        "group": task.group, "points": task.points, "done": done })
        })
        .collect()
}

fn earned(done: &[bool]) -> i64 {
    TASKS
        .iter()
        .zip(done)
        .filter(|(_, done)| **done)
        .map(|(task, _)| task.points)
        .sum()
}

fn settled() -> Result<Option<Value>> {
    let value = sdk::host("ledger.get", json!({ "kind": "onboarding" }))?;
    let valid = value["tasks"].as_array().is_some_and(|rows| {
        rows.len() == TASKS.len()
            && TASKS
                .iter()
                .zip(rows)
                .all(|(task, row)| row["key"] == task.key && row["done"] == true)
    });
    Ok(valid.then_some(value))
}

fn memory(value: &Value) -> Value {
    json!({ "tasks": value["tasks"], "earned": value["earned"], "total": value["total"],
        "unclaimed": 0, "settled": true, "settledAt": value["at"] })
}

fn document(done: &[bool]) -> Value {
    let unclaimed = done.iter().filter(|done| !**done).count();
    json!({ "tasks": rows(done), "earned": earned(done), "total": TOTAL,
        "unclaimed": unclaimed, "settled": unclaimed == 0 })
}

fn remember(document: &mut Value, previous: Option<&Value>) {
    let snapshot = if document["settled"] == true {
        let at = previous
            .filter(|old| old["tasks"] == document["tasks"])
            .and_then(|old| old["at"].as_i64())
            .unwrap_or_else(sdk::now);
        document["settledAt"] = json!(at);
        json!({ "at": at, "tasks": document["tasks"], "earned": document["earned"], "total": TOTAL })
    } else {
        Value::Null
    };
    if sdk::host(
        "ledger.put",
        json!({ "kind": "onboarding", "value": snapshot }),
    )
    .is_err()
    {
        document["warnings"] = json!(["新手任务结算记忆未能保存，本次上游结果不受影响"]);
    }
}

fn fetch(session: &str) -> Result<Vec<bool>> {
    let payload = client::token_request(
        "GET",
        "/api/v1/onboarding/tasks",
        session,
        None,
        "新手任务查询",
    )?;
    errors::ensure_success(&payload, "新手任务查询", session)?;
    let tasks = payload
        .pointer("/data/tasks")
        .filter(|tasks| tasks.is_object())
        .ok_or_else(|| Error::http(502, "Loomy 新手任务响应缺少完成状态表"))?;
    TASKS
        .iter()
        .map(|task| {
            tasks[task.key]
                .as_bool()
                .ok_or_else(|| Error::http(502, "Loomy 新手任务响应缺少明确完成状态，未尝试领取"))
        })
        .collect()
}

pub fn status(refresh: bool) -> Result<Value> {
    let before = account::current()?;
    let _lock = AccountLock::acquire()?;
    account::ensure_current(&before)?;
    let record = account::current()?;
    let session = account::token(&record["secret"])?;
    let previous = settled()?;
    if !refresh {
        if let Some(previous) = &previous {
            account::ensure_current(&record)?;
            return Ok(memory(previous));
        }
    }
    let mut result = document(&fetch(&session)?);
    account::ensure_current(&record)?;
    remember(&mut result, previous.as_ref());
    Ok(result)
}

fn claim_one(session: &str, key: &str) -> Result<bool> {
    let payload = client::token_request(
        "POST",
        "/api/v1/onboarding/tasks/complete",
        session,
        Some(&json!({ "key": key })),
        "新手任务领取",
    )?;
    errors::ensure_success(&payload, "新手任务领取", session)?;
    payload
        .pointer("/data/alreadyCompleted")
        .and_then(Value::as_bool)
        .ok_or_else(|| Error::http(502, "Loomy 新手任务领取结果缺少确认字段，未自动重新提交"))
}

pub fn claim(only: &str) -> Result<Value> {
    if !only.is_empty() && !TASKS.iter().any(|task| task.key == only) {
        return Err(Error::http(400, "Loomy 未识别的新手任务，未提交领取"));
    }
    let before_lock = account::current()?;
    let _lock = AccountLock::acquire()?;
    account::ensure_current(&before_lock)?;
    let record = account::current()?;
    let session = account::token(&record["secret"])?;
    if let Some(previous) = settled()? {
        let mut result = memory(&previous);
        result["results"] = json!([]);
        result["claimed"] = json!(0);
        result["failed"] = json!(0);
        result["claimedPoints"] = json!(0);
        account::ensure_current(&record)?;
        return Ok(result);
    }
    let before = fetch(&session)?;
    account::ensure_current(&record)?;
    let state = state::get(Scope::Account, "onboarding-requests", 1)?;
    if !state.compatible {
        return Err(Error::http(409, "Loomy 新手领取记录格式已变化"));
    }
    let mut pending = state.value.as_object().cloned().unwrap_or_default();
    for (index, task) in TASKS.iter().enumerate() {
        if before[index] {
            pending.remove(task.key);
        }
    }
    let mut revision = state.revision;
    let mut after = before.clone();
    let mut results = Vec::new();
    let mut claimed = 0_i64;
    let mut failed = 0_i64;
    let mut claimed_points = 0_i64;
    for (index, task) in TASKS.iter().enumerate() {
        if before[index] || !only.is_empty() && only != task.key {
            continue;
        }
        if pending.contains_key(task.key) {
            failed += 1;
            results.push(json!({ "key": task.key, "ok": false,
                "error": "此前领取结果尚未确认，已只读查任务状态但未重复提交" }));
            continue;
        }
        account::ensure_current(&record)?;
        pending.insert(task.key.into(), json!({ "startedAt": sdk::now() }));
        if !state::compare_exchange(
            Scope::Account,
            "onboarding-requests",
            1,
            revision,
            Value::Object(pending.clone()),
            0,
        )? {
            return Err(Error::http(409, "Loomy 新手任务记录已变化，未覆盖其它操作"));
        }
        revision = state::get(Scope::Account, "onboarding-requests", 1)?.revision;
        let reply = claim_one(&session, task.key);
        account::ensure_current(&record)?;
        match reply {
            Ok(already) => {
                pending.remove(task.key);
                after[index] = true;
                claimed += 1;
                // 并发或其他客户端已领取时只确认完成，不把旧奖励当成本次新赚。
                if !already {
                    claimed_points += task.points;
                }
                results.push(json!({ "key": task.key, "ok": true, "already": already, "points": task.points }));
            }
            Err(error) if error.status == Some(401) => return Err(error),
            Err(error) => {
                if matches!(error.status, Some(400 | 403 | 429)) {
                    pending.remove(task.key);
                }
                failed += 1;
                results.push(json!({ "key": task.key, "ok": false, "error": error.message }));
            }
        }
        if !state::compare_exchange(
            Scope::Account,
            "onboarding-requests",
            1,
            revision,
            Value::Object(pending.clone()),
            0,
        )? {
            return Err(Error::http(
                409,
                "Loomy 领取结果已返回但台账有冲突，未重复领取",
            ));
        }
        revision = state::get(Scope::Account, "onboarding-requests", 1)?.revision;
    }
    account::ensure_current(&record)?;
    let mut result = document(&after);
    result["results"] = json!(results);
    result["claimed"] = json!(claimed);
    result["failed"] = json!(failed);
    result["claimedPoints"] = json!(claimed_points);
    remember(&mut result, None);
    Ok(result)
}
