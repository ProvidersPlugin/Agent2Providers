//! 对外目录按国内 provider 持久化；签名参数、enable 权益位只在账号绑定 state 中。
//! 不声明 catalog.accounts.v1：全量可见目录（含未开通）不是账号使用许可名单。
use super::{account, catalog, client, cosy, endpoints, refresh, util};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    Error, Result, Value,
};

const KEY: &str = "qoder-route-catalog";
const TTL: i64 = 3_600_000;
const BACKOFF: &str = "catalog-refresh";

pub fn list() -> Result<Value> {
    let cached = sdk::catalog::cached()?;
    if cached["meta"]["remote"] == true && cached["models"].is_array() {
        return Ok(cached);
    }
    Ok(
        json!({ "models": catalog::public(&catalog::fallback()), "meta": { "remote": false, "refreshed_at": 0 } }),
    )
}
fn cache() -> Result<state::Snapshot> {
    state::get(Scope::Account, KEY, 1)
}
fn cached(
    before: &state::Snapshot,
    snapshot: &account::Snapshot,
    fresh: bool,
) -> Result<Option<Vec<Value>>> {
    if !before.compatible || before.value["binding"] != snapshot.credentials.binding() {
        return Ok(None);
    }
    if fresh
        && !(0..TTL)
            .contains(&util::now()?.saturating_sub(before.value["at"].as_i64().unwrap_or(0)))
    {
        return Ok(None);
    }
    Ok(before.value["models"].as_array().cloned())
}
fn fetch(snapshot: &account::Snapshot) -> Result<Vec<Value>> {
    let headers = cosy::headers(None, endpoints::MODELS, &snapshot.credentials)?;
    let reply = client::request(
        "GET",
        endpoints::GATEWAY,
        endpoints::MODELS,
        headers,
        None,
        "模型目录查询",
    )?;
    let payload = client::accepted(reply, "模型目录查询")?;
    catalog::parse(&payload)
}
fn store(before: &state::Snapshot, snapshot: &account::Snapshot, models: &[Value]) -> Result<()> {
    account::unchanged(snapshot)?;
    let value =
        json!({ "binding": snapshot.credentials.binding(), "at": util::now()?, "models": models });
    if value.to_string().len() > 240 * 1024 {
        return Err(Error::http(
            502,
            "Qoder 模型目录超过缓存容量，未替换现有目录",
        ));
    }
    if !state::compare_exchange(Scope::Account, KEY, 1, before.revision, value, 0)? {
        return Err(Error::http(409, "Qoder 目录已被另一轮刷新，请重试"));
    }
    Ok(())
}
pub fn refresh(params: &Value) -> Result<Value> {
    if params["account"].as_str().is_none_or(str::is_empty) {
        return Ok(
            json!({ "refreshed": false, "message": "没有主程序选定的 Qoder 中国版账号，已保留静态目录" }),
        );
    }
    let result = (|| -> Result<Value> {
        let snapshot = refresh::ensure(false)?;
        let before = cache()?;
        if !params["force"].as_bool().unwrap_or(false) {
            if let Some(models) = cached(&before, &snapshot, true)? {
                // 宿主公开快照没有这批条目时仍补交；时间沿用真实读取时刻，不假刷新。
                let public = catalog::public(&models);
                let shown = list()?;
                account::unchanged(&snapshot)?;
                if shown["models"] == json!(public) && shown["meta"]["remote"] == true {
                    return Ok(sdk::catalog::unchanged(models.len()));
                }
                return Ok(
                    json!({ "refreshed": true, "count": public.len(), "models": public,
                    "meta": { "remote": true, "refreshed_at": before.value["at"] } }),
                );
            }
        }
        let models = fetch(&snapshot)?;
        store(&before, &snapshot, &models)?;
        Ok(sdk::catalog::refreshed(catalog::public(&models)))
    })();
    match result {
        Ok(value) => Ok(value),
        Err(error) if util::terminal(&error) => Err(error),
        Err(error) => Ok(sdk::catalog::failed(&error.message)),
    }
}
pub fn for_account(snapshot: &account::Snapshot) -> Result<Vec<Value>> {
    let before = cache()?;
    if let Some(models) = cached(&before, snapshot, true)? {
        return Ok(models);
    }
    let previous = cached(&before, snapshot, false)?;
    let backoff = util::transient(BACKOFF)?;
    let now = util::now()?;
    if backoff.compatible
        && backoff.value["binding"] == snapshot.credentials.binding()
        && backoff.value["retryAt"].as_i64().unwrap_or(0) > now
    {
        return Ok(previous.unwrap_or_else(catalog::fallback));
    }
    if !util::put(
        BACKOFF,
        &backoff,
        json!({ "binding": snapshot.credentials.binding(), "retryAt": now + 60_000 }),
        TTL,
    )? {
        return Ok(previous.unwrap_or_else(catalog::fallback));
    }
    match fetch(snapshot) {
        Ok(models) => {
            store(&before, snapshot, &models)?;
            Ok(models)
        }
        Err(error) if util::terminal(&error) => Err(error),
        Err(_) => {
            account::unchanged(snapshot)?;
            Ok(previous.unwrap_or_else(catalog::fallback))
        }
    }
}
pub fn resolve(name: &str, snapshot: &account::Snapshot) -> Result<Value> {
    if name.trim().is_empty() || name.len() > 512 || name.chars().any(char::is_control) {
        return Err(Error::http(400, "Qoder 请指定当前国内目录中的有效模型名"));
    }
    let models = for_account(snapshot)?;
    catalog::find(&models, name).ok_or_else(|| {
        Error::http(400, "模型不在 Qoder 中国版当前目录中，请刷新目录后重试")
            .with_code("model_not_found")
    })
}
