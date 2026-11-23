//! 通用账号台账与有限任务进度。所有存储绑定当前授权身份，不依赖进程缓存。
use super::{
    account,
    session::{self, Session},
    util,
};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope, Snapshot},
    Error, Result, Value,
};

pub fn load(key: &str) -> Result<Snapshot> {
    let value = state::get(Scope::Account, key, 1)?;
    if !value.compatible {
        return Err(Error::http(409, "WorkBuddy状态版本不兼容"));
    }
    Ok(value)
}
pub fn store(key: &str, before: &Snapshot, value: Value) -> Result<()> {
    if !state::compare_exchange(Scope::Account, key, 1, before.revision, value, 0)? {
        return Err(Error::http(409, "WorkBuddy状态已被另一调用更新"));
    }
    Ok(())
}
pub fn get(current: &Session, kind: &str) -> Result<Value> {
    session::check(current)?;
    let key = format!("ledger-{kind}");
    let state = load(&key)?;
    let binding = account::fingerprint(&current.secret);
    if state.value["binding"]
        .as_str()
        .is_some_and(|v| v != binding)
    {
        return Ok(Value::Null);
    }
    if let Some(value) = state.value.get("value") {
        return Ok(value.clone());
    }
    let legacy = sdk::host("ledger.get", json!({ "kind": kind }))?;
    session::check(current)?;
    store(&key, &state, json!({ "binding": binding, "value": legacy }))?;
    Ok(legacy)
}
pub fn put(current: &Session, kind: &str, value: &Value) -> Result<()> {
    session::check(current)?;
    let key = format!("ledger-{kind}");
    let state = load(&key)?;
    store(
        &key,
        &state,
        json!({ "binding": account::fingerprint(&current.secret), "value": value }),
    )?;
    session::check(current)?;
    sdk::host("ledger.put", json!({ "kind": kind, "value": value }))?;
    Ok(())
}
pub fn record(current: &Session, action: &str, report: &mut Value) {
    let mut ledger = match get(current, "growth-rewards") {
        Ok(v) => v,
        Err(_) => {
            util::warning(report, "奖励结果已返回，但旧台账未能读取");
            return;
        }
    };
    if !ledger.is_object() {
        ledger = json!({});
    }
    let mut rows = ledger["rows"].as_array().cloned().unwrap_or_default();
    rows.push(json!({ "at": sdk::now(), "action": action, "result": report }));
    if rows.len() > 48 {
        rows.drain(..rows.len() - 48);
    }
    ledger["rows"] = json!(rows);
    while ledger.to_string().len() > 60 * 1024
        && ledger["rows"].as_array().is_some_and(|v| v.len() > 1)
    {
        if let Some(rows) = ledger["rows"].as_array_mut() {
            rows.remove(0);
        }
    }
    if put(current, "growth-rewards", &ledger).is_err() {
        util::warning(report, "本次官方结果不受影响，但奖励台账未保存");
    }
}

/// 长任务显式启动时获取租约；取消不取账号锁，当前调用在每个网络/等待边界检查。
pub struct Job {
    generation: String,
    binding: String,
    pub until: i64,
}
impl Job {
    pub fn begin(current: &Session, label: &str, requested_until: i64) -> Result<Self> {
        session::check(current)?;
        let until = requested_until.min(sdk::now().saturating_add(480_000));
        if until <= sdk::now() {
            return Err(budget_exhausted());
        }
        let before = load("job-control")?;
        let generation = util::random_hex(16)?;
        let binding = account::fingerprint(&current.secret);
        store(
            "job-control",
            &before,
            json!({ "generation": generation, "binding": binding, "action": label, "running": true, "cancelled": false, "until": until }),
        )?;
        Ok(Self {
            generation,
            binding,
            until,
        })
    }
    pub fn check(&self) -> Result<()> {
        let value = load("job-control")?.value;
        if value["generation"] != self.generation
            || value["binding"] != self.binding
            || value["cancelled"] == true
        {
            return Err(Error::http(409, "WorkBuddy任务已取消或被替换").with_code("task_cancelled"));
        }
        if sdk::now() >= self.until {
            return Err(budget_exhausted());
        }
        let current = session::snapshot()?;
        if account::fingerprint(&current.secret) != self.binding {
            return Err(Error::http(409, "任务执行期间账号身份已变化"));
        }
        Ok(())
    }
    pub fn pause(&self, millis: u64) -> Result<()> {
        self.check()?;
        if self.until.saturating_sub(sdk::now()) <= i64::try_from(millis).unwrap_or(i64::MAX) {
            return Err(budget_exhausted());
        }
        sdk::sleep(millis)?;
        self.check()
    }
}
fn budget_exhausted() -> Error {
    Error::http(408, "本轮任务已到有限执行时长，下次从官方状态继续")
        .with_code("task_budget_exhausted")
}
impl Drop for Job {
    fn drop(&mut self) {
        if let Ok(before) = load("job-control") {
            if before.value["generation"] == self.generation {
                let mut value = before.value.clone();
                value["running"] = json!(false);
                value["finishedAt"] = json!(sdk::now());
                let _ = store("job-control", &before, value);
            }
        }
    }
}
pub fn cancel() -> Result<Value> {
    account::record()?;
    for _ in 0..3 {
        let before = load("job-control")?;
        let mut value = before.value.clone();
        if !value.is_object() {
            return Ok(json!({ "cancelled": false, "message": "当前没有运行中的任务" }));
        }
        value["cancelled"] = json!(true);
        if store("job-control", &before, value).is_ok() {
            return Ok(
                json!({ "cancelled": true, "message": "已请求在下一安全步骤停止；正在等待的网络请求仍由宿主超时结束" }),
            );
        }
    }
    Err(Error::http(409, "任务状态正在变化，取消未确认"))
}
pub fn status() -> Result<Value> {
    let current = session::snapshot()?;
    let mut value = load("job-control")?.value;
    if value["binding"] != account::fingerprint(&current.secret) {
        return Ok(Value::Null);
    }
    if value.is_object() && value["until"].as_i64().is_some_and(|at| at <= sdk::now()) {
        value["running"] = json!(false);
    }
    Ok(value)
}
