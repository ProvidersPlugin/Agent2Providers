//! 账号目录10分钟TTL；失败保留旧快照。凭据/项目指纹隔离，不能复用别号Argon证据。
//! 原生目录不是完整权限表：仅 catalog.v1 共享展示；账号真实证据独立保存，不广告 catalog.accounts.v1。

use super::{account, argon, catalog_shape, control, credentials, errors, project, refresh, util};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{Scope, Snapshot},
    AccountLock, Error, Result, Value,
};

const KEY: &str = "model-catalog";
const TTL: i64 = 600_000;
const FORMAT: u64 = 2;

fn snapshot() -> Result<Snapshot> {
    let state = sdk::state::get(Scope::Account, KEY, 1)?;
    if !state.compatible {
        return Err(Error::http(409, "Antigravity 模型缓存版本变化，请重新刷新"));
    }
    Ok(state)
}
fn cached(state: &Snapshot, secret: &Value, fresh: bool) -> Option<(Vec<Value>, i64)> {
    if state.value["binding"] != credentials::binding(secret) {
        return None;
    }
    let current_format = state.value["format"].as_u64() == Some(FORMAT);
    if fresh && !current_format {
        return None;
    }
    let at = state.value["at"].as_i64()?;
    if fresh && !(0..TTL).contains(&util::now().ok()?.saturating_sub(at)) {
        return None;
    }
    let models = state.value["models"].as_array()?.clone();
    let models = if current_format {
        models
    } else {
        catalog_shape::collapse_cached_entries(models)
    };
    Some((models, at))
}

pub fn list(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        let mut shared = sdk::catalog::cached()?;
        if shared["meta"]["remote"] == true && shared["models"].is_array() {
            if let Some(models) = shared["models"].as_array().cloned() {
                if catalog_shape::needs_collapse(&models) {
                    shared["models"] = json!(catalog_shape::collapse_cached_entries(models));
                }
            }
            return Ok(shared);
        }
        return Ok(
            json!({ "models": catalog_shape::fallback(), "meta": { "remote": false, "refreshed_at": 0 } }),
        );
    }
    let record = account::current()?;
    let secret = credentials::parse(&record["secret"])?;
    Ok(match cached(&snapshot()?, &secret, false) {
        Some((models, at)) => {
            json!({ "models": models, "meta": { "remote": true, "refreshed_at": at } })
        }
        None => {
            json!({ "models": catalog_shape::fallback(), "meta": { "remote": false, "refreshed_at": 0 } })
        }
    })
}

fn load_or_fetch(record: &Value, force: bool) -> Result<(Vec<Value>, i64, bool)> {
    let secret = credentials::parse(&record["secret"])?;
    let old = snapshot()?;
    if !force {
        if let Some((models, at)) = cached(&old, &secret, true) {
            return Ok((models, at, false));
        }
    }
    let body = control::fetch(&secret, false, || account::unchanged(&record["secret"]))?;
    let mut models = catalog_shape::parse(&body);
    account::unchanged(&record["secret"])?;
    if !body["models"]
        .as_object()
        .is_some_and(|items| items.is_empty())
        && !body["models"]
            .as_array()
            .is_some_and(|items| items.is_empty())
    {
        let summary = if models
            .iter()
            .any(|model| model["id"].as_str().is_some_and(argon::is_model))
        {
            None
        } else {
            control::fetch(&secret, true, || account::unchanged(&record["secret"])).ok()
        };
        account::unchanged(&record["secret"])?;
        argon::supplement(&mut models, summary.as_ref());
    }
    if models.len() > 4096 {
        return Err(Error::http(
            502,
            "Antigravity 目录超出条目上限，未覆盖旧清单",
        ));
    }
    let at = util::now()?;
    let value = json!({ "format": FORMAT, "binding": credentials::binding(&secret), "at": at, "models": models });
    if value.to_string().len() > 240 * 1024 {
        return Err(Error::http(
            413,
            "Antigravity 模型清单超过当前主程序缓存容量，未覆盖旧清单",
        )
        .with_code("plugin_resource_limit"));
    }
    if !sdk::state::compare_exchange(Scope::Account, KEY, 1, old.revision, value, 0)? {
        return Err(Error::http(409, "Antigravity 目录被另一任务刷新，请重试"));
    }
    Ok((models, at, true))
}

pub fn refresh(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        return Ok(sdk::catalog::unchanged(catalog_shape::fallback().len()));
    }
    let record = refresh::ensure(false, None)?;
    let _lock = AccountLock::acquire()?;
    account::unchanged(&record["secret"])?;
    let record = project::locked(record, false)?;
    match load_or_fetch(&record, params["force"] == true) {
        Ok((models, at, fetched)) => {
            let host = sdk::catalog::cached()?;
            let host_stale = host["models"]
                .as_array()
                .is_some_and(|rows| catalog_shape::needs_collapse(rows));
            Ok(json!({
                "refreshed": fetched || host_stale || host["meta"]["refreshed_at"].as_i64() != Some(at),
                "count": models.len(),
                "models": models,
                "meta": { "remote": true, "refreshed_at": at }
            }))
        }
        Err(error) if errors::local(&error) => Err(error),
        Err(error) => {
            let secret = credentials::parse(&record["secret"])?;
            let count = cached(&snapshot()?, &secret, false)
                .map(|(m, _)| m.len())
                .unwrap_or_else(|| catalog_shape::fallback().len());
            Ok(json!({ "refreshed": false, "count": count, "message": error.message }))
        }
    }
}

/// 原生 chat 只读目录预算，不在每次转发额外发目录请求；自动刷新由主程序调度。
pub fn for_account(record: &Value) -> Result<Vec<Value>> {
    let secret = credentials::parse(&record["secret"])?;
    Ok(cached(&snapshot()?, &secret, false)
        .map(|(models, _)| models)
        .unwrap_or_else(catalog_shape::fallback))
}

pub fn for_request(record: &Value, wire_model: &str) -> Result<Vec<Value>> {
    if !argon::is_model(wire_model) {
        return for_account(record);
    }
    let _lock = AccountLock::acquire()?;
    account::unchanged(&record["secret"])?;
    // 共享展示目录不能提供本账号资格；过期/换凭据/换项目后必须重取真实证据。
    let (models, _, _) = load_or_fetch(record, false)?;
    if !models
        .iter()
        .any(|entry| entry["id"].as_str().is_some_and(argon::is_model))
    {
        return Err(
            Error::http(403, "Antigravity 当前账号没有 Argon 的目录或专属额度证据")
                .with_code("local_precondition"),
        );
    }
    Ok(models)
}
