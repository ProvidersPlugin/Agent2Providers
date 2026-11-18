//! 国际账号独立目录：成功空清单与失败分开，展示聚合由宿主账号快照负责。
//! 只迁本账号旧 global 台账，不使用其他账号的展示缓存替代可用模型。

use crate::{endpoints, errors, models, network, refresh, util};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope, Snapshot},
    AccountLock, Error, Result, Value,
};

const KEY: &str = "accio.catalog";
const TTL: i64 = 3_600_000;
const BACKOFF: i64 = 30_000;

fn binding(session: &refresh::Session) -> String {
    let identity = if session.credentials.user.is_empty() {
        util::fingerprint(&session.credentials.access)
    } else {
        session.credentials.user.clone()
    };
    util::fingerprint(
        &json!([endpoints::EDITION, identity, session.credentials.device]).to_string(),
    )
}

fn list_value(mut rows: Vec<Value>, remote: bool, at: i64) -> Value {
    for row in &mut rows {
        row["accioEdition"] = json!(endpoints::EDITION);
    }
    json!({ "models": rows, "meta": { "remote": remote, "refreshed_at": at } })
}

fn fallback() -> Value {
    list_value(models::fallback(), false, 0)
}

fn stored(value: &Value) -> Option<Value> {
    let rows = value["models"].as_array()?;
    if rows.len() > 512
        || rows
            .iter()
            .any(|row| row["accioEdition"] != endpoints::EDITION)
        || rows.is_empty() && value["meta"]["remote"] != true
    {
        return None;
    }
    Some(list_value(
        models::saved_rows(&value["models"])?,
        value["meta"]["remote"] == true,
        value["meta"]["refreshed_at"].as_i64().unwrap_or(0),
    ))
}

fn put(snapshot: &Snapshot, value: Value) -> Result<()> {
    if !state::compare_exchange(Scope::Account, KEY, 1, snapshot.revision, value, 0)? {
        return Err(Error::http(409, "Accio 模型状态已被其他操作更新，请重试"));
    }
    Ok(())
}

fn snapshot(session: &refresh::Session) -> Result<Snapshot> {
    let current = state::get(Scope::Account, KEY, 1)?;
    if current.compatible && current.value["binding"] == binding(session) {
        return Ok(current);
    }
    // 台账只用于恢复原账号；旧格式没有绑定信息，不可拿来跳过本次在线查询。
    let mut legacy = if current.value.is_null() {
        let old = sdk::host("ledger.get", json!({ "kind": "accio-catalog" }))?;
        models::saved_rows(&old["global"])
            .filter(|rows| !rows.is_empty())
            .map(|rows| list_value(rows, true, old["globalFetchedAt"].as_i64().unwrap_or(0)))
    } else {
        None
    };
    if let Some(list) = &mut legacy {
        list["meta"]["refreshed_at"] = json!(0);
    }
    refresh::check(session)?;
    put(
        &current,
        json!({ "binding": binding(session), "list": legacy.unwrap_or_else(fallback),
        "retry_at": 0 }),
    )?;
    state::get(Scope::Account, KEY, 1)
}

fn fetch(session: &refresh::Session) -> Result<(Value, usize)> {
    let credential = &session.credentials;
    let body = json!({ "token": credential.access, "supportAutoModel": true });
    let mut last = Error::http(502, "Accio 上游目录结构无效");
    for (version, path) in endpoints::MODELS.iter().enumerate() {
        refresh::check(session)?;
        let payload = match network::post(path, &body, &credential.secrets()) {
            Ok(value) => value,
            Err(error) if errors::host_stop(&error) || matches!(error.status, Some(401 | 429)) => {
                return Err(error)
            }
            Err(error) => {
                last = error;
                continue;
            }
        };
        refresh::check(session)?;
        match models::parse_checked(&payload) {
            Ok(rows) => return Ok((list_value(rows, true, util::now()?), version)),
            Err(error) => last = error,
        }
    }
    Err(last)
}

struct Loaded {
    list: Value,
    fetched: bool,
    message: Option<String>,
}

fn load(session: &refresh::Session, force: bool) -> Result<Loaded> {
    let _lock = AccountLock::acquire()?;
    refresh::check(session)?;
    let snapshot = snapshot(session)?;
    let previous = stored(&snapshot.value["list"]).unwrap_or_else(fallback);
    let now = util::now()?;
    let age = now.saturating_sub(previous["meta"]["refreshed_at"].as_i64().unwrap_or(0));
    if !force && previous["meta"]["remote"] == true && (0..TTL).contains(&age) {
        return Ok(Loaded {
            list: previous,
            fetched: false,
            message: None,
        });
    }
    if !force
        && snapshot.value["retry_at"]
            .as_i64()
            .is_some_and(|at| at > now)
    {
        return Ok(Loaded {
            list: previous,
            fetched: false,
            message: Some("Accio 模型目录暂不可用，保留本账号最近清单，稍后重试".into()),
        });
    }
    match fetch(session) {
        Ok((list, version)) => {
            refresh::check(session)?;
            put(
                &snapshot,
                json!({ "binding": binding(session), "list": list, "retry_at": 0 }),
            )?;
            Ok(Loaded {
                list,
                fetched: true,
                message: (version == 1).then(|| "完整目录暂不可用，本次使用 v2 精简目录".into()),
            })
        }
        Err(error) if errors::host_stop(&error) || error.status == Some(401) => Err(error),
        Err(error) => {
            refresh::check(session)?;
            put(
                &snapshot,
                json!({ "binding": binding(session), "list": previous,
                "retry_at": now.saturating_add(BACKOFF) }),
            )?;
            Ok(Loaded {
                list: previous,
                fetched: false,
                message: Some(errors::redact(
                    &error.message,
                    &session.credentials.secrets(),
                )),
            })
        }
    }
}

pub fn list(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        return Ok(fallback());
    }
    let session = refresh::snapshot()?;
    let list = load(&session, false)?.list;
    refresh::check(&session)?;
    Ok(list)
}

pub fn for_chat(session: &refresh::Session, model: &str) -> Result<Value> {
    let loaded = load(session, false)?;
    refresh::check(session)?;
    let rows = loaded.list["models"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    models::resolve(&rows, model).ok_or_else(|| {
        Error::http(
            400,
            "Accio 当前账号目录中没有该模型，请刷新目录并使用清单中的型号",
        )
        .with_code("model_not_found")
    })
}

pub fn refresh(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        let mut list = fallback();
        list["refreshed"] = json!(false);
        list["count"] = json!(list["models"].as_array().map(Vec::len).unwrap_or(0));
        return Ok(list);
    }
    let mut session = refresh::ready()?;
    let force = params["force"].as_bool().unwrap_or(false);
    let loaded = match load(&session, force) {
        Err(error) if error.status == Some(401) && !session.credentials.refresh.is_empty() => {
            session = refresh::after_rejection(&session.credentials)?;
            load(&session, force)?
        }
        result => result?,
    };
    refresh::check(&session)?;
    let mut list = loaded.list;
    // 缓存也可向缺少宿主快照的账号同步；失败绝不借旧 remote=true 假报刷新。
    let host = sdk::catalog::cached()?;
    list["refreshed"] = json!(
        loaded.fetched
            || loaded.message.is_none()
                && list["meta"]["remote"] == true
                && stored(&host).as_ref() != Some(&list)
    );
    list["count"] = json!(list["models"].as_array().map(Vec::len).unwrap_or(0));
    if let Some(message) = loaded.message {
        list["message"] = json!(message);
    }
    Ok(list)
}
