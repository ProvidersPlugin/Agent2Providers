//! 展示目录与 COSY 路由素材都按原账号保存，由宿主聚合；权益不能来自其他账号。
use super::{account, catalog, client, cosy, endpoints, refresh, util};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    Error, Result, Value,
};

const KEY: &str = "qwenwork-cn-route-catalog";
const TTL: i64 = 3_600_000;
const BACKOFF: &str = "catalog-refresh";

pub fn list(params: &Value) -> Result<Value> {
    if params["account"].as_str().is_none_or(str::is_empty) {
        return Ok(json!({ "models": catalog::public(&catalog::fallback()),
            "meta": { "remote": false, "refreshed_at": 0 } }));
    }
    let snapshot = account::read()?;
    let before = cache()?;
    let models = cached(&before, &snapshot, false)?;
    let result = if let Some(models) = models {
        json!({ "models": catalog::public(&models), "meta": { "remote": true, "refreshed_at": before.value["at"] } })
    } else {
        let cached = sdk::catalog::cached()?;
        if cached["meta"]["remote"] == true && cached["models"].is_array() {
            cached
        } else {
            json!({ "models": catalog::public(&catalog::fallback()), "meta": { "remote": false, "refreshed_at": 0 } })
        }
    };
    account::unchanged(&snapshot)?;
    Ok(result)
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
    account::unchanged(snapshot)?;
    let headers = cosy::headers(None, endpoints::MODELS, &snapshot.credentials)?;
    let reply = client::request("GET", endpoints::MODELS, headers, None, "模型目录查询")?;
    let payload = client::accepted(reply, "模型目录查询")?;
    catalog::parse(&payload)
}
fn store(before: &state::Snapshot, snapshot: &account::Snapshot, models: &[Value]) -> Result<i64> {
    account::unchanged(snapshot)?;
    let at = util::now()?;
    let value = json!({ "binding": snapshot.credentials.binding(), "at": at, "models": models });
    if value.to_string().len() > 240 * 1024 {
        return Err(Error::http(502, "千问办公目录超过缓存容量，未替换现有目录"));
    }
    if !state::compare_exchange(Scope::Account, KEY, 1, before.revision, value, 0)? {
        return Err(Error::http(409, "千问办公目录已被另一轮刷新，请重试"));
    }
    Ok(at)
}
pub fn refresh(params: &Value) -> Result<Value> {
    if params["account"].as_str().is_none_or(str::is_empty) {
        return Ok(
            json!({ "refreshed": false, "message": "没有主程序选定的千问办公国内账号，已保留静态目录" }),
        );
    }
    let result = (|| -> Result<Value> {
        let mut snapshot = refresh::ensure(false)?;
        let before = cache()?;
        if !params["force"].as_bool().unwrap_or(false) {
            if let Some(models) = cached(&before, &snapshot, true)? {
                let public = catalog::public(&models);
                let shown = sdk::catalog::cached()?;
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
        let models = match fetch(&snapshot) {
            Err(error) if error.status == Some(401) && snapshot.credentials.can_refresh() => {
                account::unchanged(&snapshot)?;
                let original = snapshot.credentials.binding();
                snapshot = refresh::ensure(true)?;
                if snapshot.credentials.binding() != original {
                    return Err(account::changed());
                }
                fetch(&snapshot)?
            }
            result => result?,
        };
        let at = store(&before, &snapshot, &models)?;
        let public = catalog::public(&models);
        Ok(
            json!({ "refreshed": true, "count": public.len(), "models": public, "meta": { "remote": true, "refreshed_at": at } }),
        )
    })();
    match result {
        Ok(value) => Ok(value),
        Err(error) if util::terminal(&error) => Err(error),
        Err(error) => Ok(sdk::catalog::failed(&error.message)),
    }
}
pub fn for_account(snapshot: &account::Snapshot) -> Result<Vec<Value>> {
    account::unchanged(snapshot)?;
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
        return previous.ok_or_else(|| {
            Error::http(
                503,
                "当前千问办公账号目录尚未取得，请稍后刷新；未使用其他账号清单",
            )
            .with_code("account_model_permissions_unknown")
        });
    }
    if !util::put(
        BACKOFF,
        &backoff,
        json!({ "binding": snapshot.credentials.binding(), "retryAt": now + 60_000 }),
        TTL,
    )? {
        return previous.ok_or_else(|| {
            Error::http(
                503,
                "当前千问办公账号目录尚未取得，请稍后刷新；未使用其他账号清单",
            )
            .with_code("account_model_permissions_unknown")
        });
    }
    match fetch(snapshot) {
        Ok(models) => {
            store(&before, snapshot, &models)?;
            Ok(models)
        }
        Err(error) if util::terminal(&error) => Err(error),
        Err(error) => previous.ok_or(error),
    }
}
pub fn resolve(name: &str, snapshot: &account::Snapshot) -> Result<Value> {
    if name.trim().is_empty() || name.len() > 512 || name.chars().any(char::is_control) {
        return Err(Error::http(400, "请指定千问办公国内目录中的有效模型名"));
    }
    let models = for_account(snapshot)?;
    let found = catalog::find(&models, name)
        .or_else(|| {
            if models.is_empty() {
                return None;
            }
            let static_model = catalog::find(&catalog::fallback(), name)?;
            catalog::find(&models, static_model["upstreamKey"].as_str()?)
        })
        .ok_or_else(|| {
            Error::http(400, "模型不在当前千问办公国内目录中，请刷新目录后重试")
                .with_code("model_not_found")
        })?;
    if found["permissionKnown"] == true && found["accountEnabled"] != true {
        return Err(Error::http(403, "当前千问办公账号套餐不支持此模型")
            .with_code("account_model_unavailable"));
    }
    account::unchanged(snapshot)?;
    Ok(found)
}
