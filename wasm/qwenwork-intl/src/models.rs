//! 展示目录由 catalog.accounts.v1 按国际账号保存聚合，签名/路由素材仍在原账号 state。
//! qwork 展示与当前账号 enable 权益分开，合法名称在插件内解析，不借另一账号的目录。
use super::{account, catalog, client, cosy, endpoints, refresh, util};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    Error, Result, Value,
};

const KEY: &str = "qwenwork-intl-route-catalog";
const TTL: i64 = 3_600_000;
const BACKOFF: &str = "catalog-refresh";

pub fn list(params: &Value) -> Result<Value> {
    if params["account"].as_str().is_none_or(str::is_empty) {
        return Ok(
            json!({ "models": catalog::public(&catalog::fallback()), "meta": { "remote": false, "refreshed_at": 0 } }),
        );
    }
    let snapshot = account::read()?;
    let cached = sdk::catalog::cached()?;
    account::unchanged(&snapshot)?;
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
    account::unchanged(snapshot)?;
    let headers = cosy::headers(None, endpoints::MODELS, &snapshot.credentials)?;
    let reply = client::request("GET", endpoints::MODELS, headers, None, "模型目录查询")?;
    let payload = client::accepted(reply, "模型目录查询")?;
    account::unchanged(snapshot)?;
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
            json!({ "refreshed": false, "message": "没有主程序选定的千问办公国际账号，已保留静态目录" }),
        );
    }
    let result = (|| -> Result<Value> {
        let mut snapshot = refresh::ensure(false)?;
        let before = cache()?;
        if !params["force"].as_bool().unwrap_or(false) {
            if let Some(models) = cached(&before, &snapshot, true)? {
                let public = catalog::public(&models);
                let shown = list(params)?;
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
    let before = cache()?;
    if let Some(models) = cached(&before, snapshot, true)? {
        account::unchanged(snapshot)?;
        return Ok(models);
    }
    let previous = cached(&before, snapshot, false)?;
    let backoff = util::transient(BACKOFF)?;
    let now = util::now()?;
    if backoff.compatible
        && backoff.value["binding"] == snapshot.credentials.binding()
        && backoff.value["retryAt"].as_i64().unwrap_or(0) > now
    {
        account::unchanged(snapshot)?;
        return Ok(previous.unwrap_or_else(catalog::fallback));
    }
    if !util::put(
        BACKOFF,
        &backoff,
        json!({ "binding": snapshot.credentials.binding(), "retryAt": now + 60_000 }),
        TTL,
    )? {
        account::unchanged(snapshot)?;
        return Ok(previous.unwrap_or_else(catalog::fallback));
    }
    let result = fetch(snapshot);
    account::unchanged(snapshot)?;
    match result {
        Ok(models) => {
            store(&before, snapshot, &models)?;
            Ok(models)
        }
        Err(error) if util::terminal(&error) => Err(error),
        Err(_) => Ok(previous.unwrap_or_else(catalog::fallback)),
    }
}
pub fn resolve(name: &str, snapshot: &account::Snapshot) -> Result<Value> {
    if name.trim().is_empty() || name.len() > 512 || name.chars().any(char::is_control) {
        return Err(Error::http(400, "请指定千问办公国际目录中的有效模型名"));
    }
    let models = for_account(snapshot)?;
    let model = catalog::find(&models, name)
        .or_else(|| {
            // 成功空目录及完整远程目录不能被静态旧条目重新填回。
            (!models.is_empty()
                && models
                    .iter()
                    .all(|model| model["capabilityEvidence"] != "remote_catalog"))
            .then(|| catalog::find(&catalog::fallback(), name))
            .flatten()
        })
        .ok_or_else(|| {
            Error::http(400, "模型不在千问办公国际目录中，请刷新目录后重试")
                .with_code("model_not_found")
        })?;
    if model["permissionKnown"] == true && model["accountEnabled"] != true {
        return Err(Error::http(403, "当前千问办公国际账号的套餐未开通该模型")
            .with_code("qwenwork_quota_limited"));
    }
    account::unchanged(snapshot)?;
    Ok(model)
}
