//! 在线批量目录和原生7个function补拉；缓存/成功通道都在当前账号state中，TTL一小时。
//! catalog.accounts.v1只向主机提交对外模型，私有route与设备/凭据绑定不跨账号。

use super::{
    account,
    catalog::{self, Catalog, Route, CHAT_FUNCTIONS, MAX_SUFFIX},
    client, credentials, endpoints, errors, headers, refresh, util,
};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{Scope, Snapshot},
    AccountLock, Error, Result, Value,
};

const KEY: &str = "route-catalog";
const TTL: i64 = 3_600_000;

pub fn legacy_target(model: &str) -> Option<(&str, bool)> {
    for (suffix, max) in [
        (catalog::LEGACY_MAX_SUFFIX, true),
        (catalog::LEGACY_SUFFIX, false),
    ] {
        if let Some(cut) = model.len().checked_sub(suffix.len()) {
            if model
                .get(cut..)
                .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
            {
                return model.get(..cut).map(|base| (base, max));
            }
        }
    }
    None
}

fn snapshot() -> Result<Snapshot> {
    let state = sdk::state::get(Scope::Account, KEY, 1)?;
    if !state.compatible {
        return Err(Error::http(409, "Trae 模型缓存版本不兼容，请重新刷新"));
    }
    Ok(state)
}

fn read_cache(state: &Snapshot, secret: &Value, fresh: bool) -> Option<Catalog> {
    if state.value["binding"] != credentials::binding(secret) {
        return None;
    }
    let at = state.value["at"].as_i64()?;
    if fresh && !(0..TTL).contains(&util::now().ok()?.saturating_sub(at)) {
        return None;
    }
    serde_json::from_value(state.value["catalog"].clone()).ok()
}

pub fn list(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        return Ok(json!({ "models": [], "meta": { "remote": false, "refreshed_at": 0 } }));
    }
    let record = account::current()?;
    let secret = credentials::parse(&record["secret"])?;
    let state = snapshot()?;
    Ok(match read_cache(&state, &secret, false) {
        Some(catalog) => {
            json!({ "models": catalog.models, "meta": { "remote": true, "refreshed_at": state.value["at"] } })
        }
        None => json!({ "models": [], "meta": { "remote": false, "refreshed_at": 0 } }),
    })
}

fn request(path: &str, body: &Value, secret: &Value) -> Result<Value> {
    client::post(
        &format!("{}{path}", endpoints::AGENT),
        headers::chat(secret, false)?,
        body,
    )?
    .accepted()
}

fn fetch(secret: &Value) -> Result<Catalog> {
    let checked_request = |path: &str, body: &Value, secret: &Value| {
        let current = credentials::parse(&account::current()?["secret"])?;
        if credentials::binding(&current) != credentials::binding(secret) {
            return Err(
                Error::http(409, "Trae 模型目录读取期间凭据已变化").with_code("local_precondition")
            );
        }
        request(path, body, secret)
    };
    let body = json!({ "functions": CHAT_FUNCTIONS, "agent_type": "",
        "current_config_info": { "config_name": "", "is_custom_model": false }, "mode_type": 0,
        "access_type": 0, "ab_force_vids": "", "ab_autotest_advanced_mode": 0, "show_custom_model": true });
    let mut received = false;
    let mut last_failure = None;
    let mut groups = match checked_request(endpoints::BATCH_MODELS, &body, secret) {
        Ok(value) => {
            received = value
                .get("function_configs")
                .or_else(|| value.pointer("/data/function_configs"))
                .is_some_and(Value::is_array);
            if !received {
                last_failure = Some(Error::http(502, "Trae 批量目录没有有效清单"));
            }
            catalog::function_groups(&value)
        }
        Err(error) if errors::terminal_code(&error) => return Err(error),
        Err(error) => {
            last_failure = Some(error);
            Vec::new()
        }
    };
    for function in CHAT_FUNCTIONS {
        for group in groups.iter().filter(|group| group["function"] == function) {
            let Some(entries) = group["config_info_list"].as_array() else {
                continue;
            };
            if entries.len() > 4096 {
                return Err(Error::http(502, "Trae 单通道目录超过受控大小"));
            }
        }
        if groups.iter().any(|group| group["function"] == function) {
            continue;
        }
        let body = json!({ "function": function, "config_names": null, "need_prompt": false,
            "current_config_info": null, "poly_prompt": true, "mode_type": null, "agent_type": null });
        match checked_request(endpoints::MODELS, &body, secret) {
            Ok(value) => {
                if let Some(entries) = catalog::single_entries(&value) {
                    received = true;
                    groups.push(json!({ "function": function, "config_info_list": entries }));
                } else {
                    last_failure = Some(Error::http(502, "Trae 单通道目录没有有效清单"));
                }
            }
            Err(error) if errors::terminal_code(&error) => return Err(error),
            Err(error) => {
                last_failure = Some(error);
            }
        }
    }
    let total: usize = groups
        .iter()
        .filter_map(|g| g["config_info_list"].as_array())
        .map(Vec::len)
        .sum();
    if groups.len() > 32 || total > 4096 {
        return Err(Error::http(502, "Trae 模型目录超过受控大小"));
    }
    let catalog = catalog::parse(&groups);
    if catalog.models.is_empty() && (!received || last_failure.is_some()) {
        return Err(last_failure
            .unwrap_or_else(|| Error::http(502, "Trae 各通道目录均未确认，保留上一份清单")));
    }
    // 成功读取的空清单/全部停用与网络或格式失败不同；不能永久留旧可用目录。
    Ok(catalog)
}

fn load_or_fetch(record: &Value, force: bool) -> Result<(Catalog, i64, bool)> {
    let secret = credentials::parse(&record["secret"])?;
    let old = snapshot()?;
    if !force {
        if let Some(catalog) = read_cache(&old, &secret, true) {
            return Ok((catalog, old.value["at"].as_i64().unwrap_or(0), false));
        }
    }
    let catalog = fetch(&secret)?;
    account::unchanged(&record["secret"])?;
    let at = util::now()?;
    let value = json!({ "binding": credentials::binding(&secret), "at": at, "catalog": catalog });
    if serde_json::to_vec(&value)
        .map_err(|_| Error::http(502, "Trae 目录无法序列化"))?
        .len()
        > 240 * 1024
    {
        return Err(Error::http(
            502,
            "Trae 模型路由目录超过缓存容量，未覆盖旧清单",
        ));
    }
    if !sdk::state::compare_exchange(Scope::Account, KEY, 1, old.revision, value, 0)? {
        return Err(Error::http(409, "Trae 模型目录已被其他任务刷新，请重试"));
    }
    Ok((catalog, at, true))
}

pub fn refresh(params: &Value) -> Result<Value> {
    let record = refresh::ensure(false, None)?;
    let _lock = AccountLock::acquire()?;
    account::unchanged(&record["secret"])?;
    match load_or_fetch(&record, params["force"] == true) {
        Ok((catalog, at, refreshed)) => {
            // 私有路由缓存可能已由chat刷新，仍须把同一快照交给宿主目录。
            let host = sdk::catalog::cached()?;
            let changed = refreshed || host["meta"]["refreshed_at"].as_i64() != Some(at);
            Ok(
                json!({ "refreshed": changed, "count": catalog.models.len(), "models": catalog.models,
                "meta": { "remote": true, "refreshed_at": at } }),
            )
        }
        Err(error)
            if errors::terminal_code(&error)
                || matches!(error.code.as_deref(), Some("aborted" | "timeout")) =>
        {
            Err(error)
        }
        Err(error) => {
            let old = snapshot()?;
            let secret = credentials::parse(&record["secret"])?;
            let count = read_cache(&old, &secret, false)
                .map(|c| c.models.len())
                .unwrap_or(0);
            Ok(json!({ "refreshed": false, "count": count, "message": error.message }))
        }
    }
}

fn routes<'a>(catalog: &'a Catalog, model: &str) -> Option<&'a Vec<Route>> {
    let name = model.trim().to_lowercase();
    if let Some(routes) = catalog.routes.get(&name) {
        return Some(routes);
    }
    let (base, max) = legacy_target(&name)?;
    if max {
        catalog
            .legacy_max_routes
            .get(base)
            .or_else(|| catalog.routes.get(&format!("{base}{MAX_SUFFIX}")))
    } else {
        catalog.routes.get(base)
    }
}

pub fn for_chat(record: &Value, model: &str) -> Result<Vec<Route>> {
    let _lock = AccountLock::acquire()?;
    account::unchanged(&record["secret"])?;
    let catalog = match load_or_fetch(record, false) {
        Ok((catalog, _, _)) => catalog,
        Err(error) if errors::terminal_code(&error) => return Err(error),
        Err(error) => {
            let old = snapshot()?;
            let secret = credentials::parse(&record["secret"])?;
            match read_cache(&old, &secret, false) {
                Some(catalog) => catalog,
                None => return Err(error),
            }
        }
    };
    routes(&catalog, model)
        .filter(|r| !r.is_empty())
        .cloned()
        .ok_or_else(|| {
            Error::http(404, "Trae 当前账号目录没有此模型或对应Max档位，请刷新模型")
                .with_code("local_precondition")
        })
}

pub fn remember(secret: &Value, model: &str, route: &Route) -> Result<()> {
    let old = snapshot()?;
    let secret = credentials::parse(secret)?;
    let Some(mut catalog) = read_cache(&old, &secret, false) else {
        return Ok(());
    };
    let name = model.trim().to_lowercase();
    let target = if catalog.routes.contains_key(&name) {
        catalog.routes.get_mut(&name)
    } else if let Some((base, max)) = legacy_target(&name) {
        if max {
            catalog.legacy_max_routes.get_mut(base)
        } else {
            catalog.routes.get_mut(base)
        }
    } else {
        None
    };
    if let Some(routes) = target {
        if let Some(index) = routes.iter().position(|other| other == route) {
            let route = routes.remove(index);
            routes.insert(0, route);
        }
    }
    let mut value = old.value.clone();
    value["catalog"] =
        serde_json::to_value(catalog).map_err(|_| Error::http(502, "Trae 模型路由缓存格式错误"))?;
    let _ = sdk::state::compare_exchange(Scope::Account, KEY, 1, old.revision, value, 0)?;
    Ok(())
}
