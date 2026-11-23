//! 国际日活的私有步骤台账；每次实例可重建，账号锁之外仍用CAS和身份绑定。
use crate::{
    account,
    session::{self, Session},
    util,
};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope, Snapshot},
    AccountLock, Error, Result, Value,
};

fn load(key: &str) -> Result<Snapshot> {
    let snapshot = state::get(Scope::Account, key, 1)?;
    if !snapshot.compatible {
        return Err(Error::http(409, "国际日活状态版本不兼容"));
    }
    Ok(snapshot)
}
fn store(key: &str, before: &Snapshot, value: Value) -> Result<()> {
    if !state::compare_exchange(Scope::Account, key, 1, before.revision, value, 0)? {
        return Err(Error::http(409, "国际日活状态已被另一调用更新"));
    }
    Ok(())
}
pub fn get(current: &Session, key: &str) -> Result<Value> {
    session::check(current)?;
    let snapshot = load(&format!("intl-activity-{key}"))?;
    Ok(
        if snapshot.value["binding"] == account::fingerprint(&current.secret) {
            snapshot.value["value"].clone()
        } else {
            Value::Null
        },
    )
}
pub fn put(current: &Session, key: &str, value: &Value) -> Result<()> {
    session::check(current)?;
    let key = format!("intl-activity-{key}");
    let before = load(&key)?;
    store(
        &key,
        &before,
        json!({ "binding": account::fingerprint(&current.secret), "value": value }),
    )
}
pub fn last(current: &Session, value: &mut Value) {
    if put(current, "last", value).is_err() {
        util::warning(value, "国际日活结果已返回，但本轮台账未保存");
    }
}

pub struct Job {
    generation: String,
    binding: String,
    until: i64,
}
impl Job {
    pub fn begin(current: &Session, until: i64) -> Result<Self> {
        if until <= sdk::now() {
            return Err(budget());
        }
        session::check(current)?;
        let before = load("intl-activity-job")?;
        if before.value["running"] == true
            && before.value["cancelled"] != true
            && before.value["until"]
                .as_i64()
                .is_some_and(|until| until > sdk::now())
        {
            return Err(Error::http(409, "国际日活上一轮仍在执行，请等待完成后重试"));
        }
        let generation = util::random_hex(16)?;
        let binding = account::fingerprint(&current.secret);
        store(
            "intl-activity-job",
            &before,
            json!({ "generation": generation, "binding": binding,
            "running": true, "cancelled": false, "until": until, "startedAt": sdk::now() }),
        )?;
        Ok(Self {
            generation,
            binding,
            until,
        })
    }
    pub fn check(&self, current: &Session) -> Result<()> {
        let saved = load("intl-activity-job")?.value;
        if saved["generation"] != self.generation
            || saved["binding"] != self.binding
            || saved["cancelled"] == true
        {
            return Err(
                Error::http(409, "国际日活已取消或被另一轮替换").with_code("task_cancelled")
            );
        }
        if self.until <= sdk::now() {
            return Err(budget());
        }
        if account::fingerprint(&current.secret) != self.binding {
            return Err(Error::http(409, "国际日活账号身份已变化"));
        }
        session::check(current)
    }
    pub fn remaining_ms(&self) -> Result<u64> {
        let remaining = self.until.saturating_sub(sdk::now());
        // 宿主最短单请求为1秒，剩余不足时不能悄悄把本轮预算延长。
        if remaining < 1000 {
            return Err(budget());
        }
        Ok(remaining as u64)
    }
    pub fn checkpoint(&self, current: &Session) -> Result<()> {
        // 持久化结果不检查时间预算，但仍核对账号；发新请求必须另行check。
        if account::fingerprint(&current.secret) != self.binding {
            return Err(Error::http(409, "国际任务账号已变化"));
        }
        session::check(current)?;
        let saved = load("intl-activity-job")?.value;
        if saved["generation"] != self.generation || saved["binding"] != self.binding {
            return Err(Error::http(409, "国际日活已被另一轮替换，未改写新任务台账"));
        }
        Ok(())
    }
}
fn budget() -> Error {
    Error::http(408, "本轮国际日活到达有限执行时长，下次从已保存步骤继续")
        .with_code("task_budget_exhausted")
}
impl Drop for Job {
    fn drop(&mut self) {
        if let Ok(before) = load("intl-activity-job") {
            if before.value["generation"] == self.generation {
                let mut value = before.value.clone();
                value["running"] = json!(false);
                value["finishedAt"] = json!(sdk::now());
                let _ = store("intl-activity-job", &before, value);
            }
        }
    }
}
pub fn cancel() -> Result<Value> {
    account::record()?;
    for _ in 0..3 {
        let before = load("intl-activity-job")?;
        if !before.value.is_object() {
            return Ok(json!({ "cancelled": false, "message": "当前没有国际日活任务" }));
        }
        let mut value = before.value.clone();
        value["cancelled"] = json!(true);
        if store("intl-activity-job", &before, value).is_ok() {
            return Ok(
                json!({ "cancelled": true, "message": "已请求在下一步骤停止；在途网络仍由主程序超时结束" }),
            );
        }
    }
    Err(Error::http(409, "国际日活状态正在变化，取消未确认"))
}
fn step_summary(value: &Value) -> Value {
    if !value.is_object() {
        return Value::Null;
    }
    json!({
        "day": value["day"],
        "done": value["done"] == true,
        "pending": value["pending"] == true,
        "model": value["model"],
        "at": value["at"],
        "reason": value["reason"],
        "status": value["status"],
        "resetAt": value["resetAt"],
    })
}
pub fn status() -> Result<Value> {
    let current = session::snapshot()?;
    let mut saved = load("intl-activity-job")?.value;
    let binding = account::fingerprint(&current.secret);
    let has_job = saved["binding"] == binding;
    if has_job && saved["until"].as_i64().is_some_and(|at| at <= sdk::now()) {
        saved["running"] = json!(false);
    }
    let last = get(&current, "last")?;
    let steps = json!({
        "model": step_summary(&get(&current, "model")?),
        "web": step_summary(&get(&current, "web")?),
        "report": step_summary(&get(&current, "report")?),
    });
    if !has_job
        && last.is_null()
        && steps["model"].is_null()
        && steps["web"].is_null()
        && steps["report"].is_null()
    {
        return Ok(Value::Null);
    }
    Ok(json!({
        "job": if has_job { saved } else { Value::Null },
        "last": last,
        "steps": steps,
    }))
}
pub fn reset_pending() -> Result<Value> {
    let _lock = AccountLock::acquire()?;
    let current = session::snapshot()?;
    let now = sdk::now();
    let mut cleared = Vec::new();
    for key in ["model", "web", "report"] {
        let mut value = get(&current, key)?;
        if value["pending"] == true && value["done"] != true {
            value["pending"] = json!(false);
            value["resetAt"] = json!(now);
            put(&current, key, &value)?;
            cleared.push(key);
        }
    }
    let message = if cleared.is_empty() {
        "当前没有待解除的保活锁定状态".to_string()
    } else {
        format!("已解除保活待确认锁定（{}），可立即重试", cleared.join("、"))
    };
    Ok(json!({ "cleared": cleared, "message": message }))
}
