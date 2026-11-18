//! 目录按账号及组织state缓存10分钟；宿主展示目录可聚合，不把展示可见性当模型权限。
//! 不声明catalog.accounts.v1，避免它把隐式自定义映射变成目录强权限闸门。

use super::{endpoints, fallback, images, model_shape, network, session, util};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope, Snapshot},
    Error, Result, Value,
};

const KEY: &str = "model-catalog";
const TTL: i64 = 600_000;

pub fn fallback() -> Value {
    json!({ "models": fallback::models(), "meta": { "remote": false, "refreshed_at": 0 } })
}
fn load() -> Result<Snapshot> {
    let snapshot = state::get(Scope::Account, KEY, 1)?;
    if !snapshot.compatible {
        return Err(Error::http(409, "小浣熊目录状态格式已变化"));
    }
    Ok(snapshot)
}
fn source(session: &session::Session) -> String {
    util::hash(
        &json!({ "user": session.credential.user, "org": session.credential.org,
        "desktop": session.source.as_ref().map(|source| source.source_id.as_str()) })
        .to_string(),
    )
}
pub fn list(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        let catalog = sdk::catalog::cached()?;
        return Ok(
            if catalog["models"].is_array() && catalog["meta"]["remote"] == true {
                catalog
            } else {
                fallback()
            },
        );
    }
    let session = session::snapshot()?;
    let snapshot = load()?;
    if snapshot.value["source"] == source(&session)
        && snapshot.value["catalog"]["models"].is_array()
    {
        Ok(snapshot.value["catalog"].clone())
    } else {
        Ok(fallback())
    }
}
pub fn refresh(params: &Value) -> Result<Value> {
    let session = match session::ensure(false) {
        Ok(session) => session,
        Err(error) => {
            return Ok(json!({ "refreshed": false, "message": error.message, "count": 0 }))
        }
    };
    let old = load()?;
    let previous = if old.value["source"] == source(&session) {
        old.value["catalog"].clone()
    } else {
        fallback()
    };
    let age = sdk::now().saturating_sub(previous["meta"]["refreshed_at"].as_i64().unwrap_or(0));
    if params["force"] != true && previous["meta"]["remote"] == true && (0..TTL).contains(&age) {
        return Ok(
            json!({ "refreshed": false, "count": previous["models"].as_array().map(Vec::len).unwrap_or(0) }),
        );
    }
    let result = network::open_timeout(
        "GET",
        &format!("{}/model_catalog", endpoints::LLM),
        &session.credential.token,
        &session.credential.org,
        json!({ "Accept": "application/json" }),
        None,
        10_000,
    )
    .and_then(|response| network::read(response, 4 * 1024 * 1024))
    .and_then(|reply| network::data(reply, &[&session.credential.token], false))
    .and_then(|data| {
        let mut models = model_shape::parse(&data)?;
        if !models.iter().any(|model| model["id"] == images::MODEL) {
            models.push(images::model());
        }
        Ok(sdk::catalog::refreshed(models))
    });
    match result {
        Ok(catalog) => {
            let latest = session::snapshot()?;
            if latest.record["secret"] != session.record["secret"]
                || source(&latest) != source(&session)
            {
                return Err(Error::http(
                    409,
                    "小浣熊账号或来源在目录刷新期间已变化，未保存旧结果",
                ));
            }
            if !state::compare_exchange(
                Scope::Account,
                KEY,
                1,
                old.revision,
                json!({ "source": source(&session), "catalog": catalog }),
                0,
            )? {
                let latest = load()?;
                if latest.value["source"] == source(&session)
                    && latest.value["catalog"]["models"].is_array()
                    && latest.value["catalog"]["meta"]["remote"] == true
                {
                    let mut catalog = latest.value["catalog"].clone();
                    catalog["refreshed"] = json!(false);
                    catalog["count"] =
                        json!(catalog["models"].as_array().map(Vec::len).unwrap_or(0));
                    return Ok(catalog);
                }
                return Err(Error::http(409, "小浣熊模型目录已被其他刷新更新"));
            }
            Ok(catalog)
        }
        Err(error) => {
            let mut previous = previous;
            previous["refreshed"] = json!(false);
            previous["count"] = json!(previous["models"].as_array().map(Vec::len).unwrap_or(0));
            previous["message"] = json!(error.message);
            Ok(previous)
        }
    }
}
