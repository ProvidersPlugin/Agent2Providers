//! 国际公开目录按 provider 保存，签名素材和 enable 权益仅在账号绑定 state 中。
//! 不声明 catalog.accounts.v1：含未开通项目的全量目录不是账号许可名单。
use super::{account, catalog, client, cosy, endpoints, refresh, util};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    Error, Result, Value,
};

const KEY: &str = "qoder-intl-route-catalog";
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
fn fetch_client(snapshot: &account::Snapshot, client_type: &str) -> Result<Vec<Value>> {
    account::unchanged(snapshot)?;
    let headers = cosy::headers_for_client(None, endpoints::MODELS, &snapshot.credentials, client_type)?;
    // 目录原生固定 api3，不套用 jt- 的对话分流。
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
fn fetch(snapshot: &account::Snapshot) -> Result<Vec<Value>> {
    let cli = fetch_client(snapshot, endpoints::CLI_CLIENT_TYPE)?;
    // 部分限时/桌面专属模型（如 HungerG）仅在桌面通道下发；附加探测失败不影响 CLI 主目录。
    match fetch_client(snapshot, endpoints::APP_CLIENT_TYPE) {
        Ok(app) => Ok(catalog::merge(cli, app)),
        Err(error) if util::terminal(&error) => Err(error),
        Err(_) => Ok(cli),
    }
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
            json!({ "refreshed": false, "message": "没有主程序选定的 Qoder 国际版账号，已保留静态目录" }),
        );
    }
    let result = (|| -> Result<Value> {
        let mut snapshot = refresh::ensure(false)?;
        let before = cache()?;
        if !params["force"].as_bool().unwrap_or(false) {
            if let Some(models) = cached(&before, &snapshot, true)? {
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
        return Err(Error::http(400, "Qoder 请指定当前国际目录中的有效模型名"));
    }
    let models = for_account(snapshot)?;
    // 不查询国内或千问，也不把公共目录中的成员资格视作本账号已开通。
    catalog::find(&models, name).ok_or_else(|| {
        Error::http(400, "模型不在 Qoder 国际版当前目录中，请刷新目录后重试")
            .with_code("model_not_found")
    })
}
