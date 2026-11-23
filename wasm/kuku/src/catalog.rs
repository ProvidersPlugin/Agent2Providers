//! 目录与会话三件套按 provider+account 隔离持久化，宿主只聚合同家账号快照。
//! 远程失败保留最后成功清单，不把静态兜底标为成功，也不借另一账号目录授予访问。

use crate::{endpoints, errors, models, network, session, util};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    AccountLock, Error, Result, Value,
};
const KEY: &str = "kuku.catalog";
const TTL: i64 = 600_000;

fn cached_locked(c: &crate::credentials::Credentials) -> Result<Value> {
    let snapshot = state::get(Scope::Account, KEY, 1)?;
    if snapshot.compatible
        && snapshot.value["binding"] == util::hash(c.get("BDUSS"))
        && models::valid(&snapshot.value["list"])
    {
        return Ok(snapshot.value["list"].clone());
    }
    let value = sdk::catalog::cached()?;
    Ok(if models::valid(&value) {
        value
    } else {
        models::fallback()
    })
}
pub fn list(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        return Ok(models::fallback());
    }
    let (_, c) = session::read()?;
    cached_locked(&c)
}
pub fn for_chat(
    c: &crate::credentials::Credentials,
    requested: &str,
) -> Result<(String, Option<String>)> {
    models::resolve(&cached_locked(c)?, requested)
}
pub fn refresh(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        let mut value = list(params)?;
        value["refreshed"] = json!(false);
        return Ok(value);
    }
    let _lock = AccountLock::acquire()?;
    let (_, c) = session::read()?;
    let snapshot = state::get(Scope::Account, KEY, 1)?;
    let previous = cached_locked(&c)?;
    let age = util::now()?.saturating_sub(previous["meta"]["refreshed_at"].as_i64().unwrap_or(0));
    if params["force"] != true && previous["meta"]["remote"] == true && (0..TTL).contains(&age) {
        session::unchanged(&c)?;
        // 升级账号目录合同时，仍要把已有本账号真实快照登记给宿主，不等待 TTL。
        let host = sdk::catalog::cached()?;
        let publish = !models::valid(&host) || host["meta"]["remote"] != true;
        let mut value = previous;
        value["refreshed"] = json!(publish);
        value["count"] = json!(value["models"].as_array().map(Vec::len).unwrap_or(0));
        return Ok(value);
    }
    let result = (|| -> Result<(Value, crate::credentials::Credentials)> {
        let ready = session::ensure_locked(false)?;
        let url = format!(
            "{}{}?{}{}",
            endpoints::BASE,
            endpoints::MODELS,
            endpoints::WEB_QUERY,
            ready.triple.query()
        );
        let mut secrets = ready.credentials.secrets();
        secrets.push(&ready.triple.bdstoken);
        secrets.push(&ready.triple.uinfo);
        let value = network::get(&url, network::headers(&ready.credentials), &secrets)?;
        errors::status(&value, &secrets)?;
        Ok((models::parse(&value, util::now()?)?, ready.credentials))
    })();
    match result {
        Ok((mut list, current)) => {
            session::unchanged(&current)?;
            let binding = util::hash(current.get("BDUSS"));
            if list.to_string().len() > 256 * 1024 {
                return Err(Error::http(502, "库库目录超过持久化大小上限"));
            }
            if !state::compare_exchange(
                Scope::Account,
                KEY,
                1,
                snapshot.revision,
                json!({ "binding": binding, "list": list }),
                0,
            )? {
                return Err(Error::http(409, "库库目录已被其他操作更新，请重试"));
            }
            list["refreshed"] = json!(true);
            list["count"] = json!(list["models"].as_array().map(Vec::len).unwrap_or(0));
            Ok(list)
        }
        Err(error) if errors::host_stop(&error) => Err(error),
        Err(error) => {
            session::unchanged(&c)?;
            let mut list = previous;
            list["refreshed"] = json!(false);
            list["message"] = json!(errors::safe_message(&error.message, &c.secrets()));
            Ok(list)
        }
    }
}
