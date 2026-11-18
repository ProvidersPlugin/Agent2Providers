//! 目录/倍率按当前账号持久；catalog.accounts.v1 保存各账号快照并由宿主聚合展示。
//! OfficeAce 旧目录白名单只负责展示，白名单外模型仍可点名，不能因迁移封掉。
//! 账号内原始目录保留，探测隐藏不会毁掉下一轮探测目标。

use super::{account, client, control, credentials::Credentials, model_shape, probe, util};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope, Snapshot},
    Error, Result, Value,
};

const KEY: &str = "model-catalog";
const TTL: i64 = 10 * 60 * 1000;

pub fn empty() -> Value {
    json!({ "models": [], "meta": { "remote": false, "refreshed_at": 0 } })
}
pub fn binding(credential: &Credentials) -> String {
    util::fingerprint(&format!(
        "{}|{}",
        credential.base_url, credential.model_app_key
    ))
}

fn snapshot() -> Result<Snapshot> {
    let state = state::get(Scope::Account, KEY, 1)?;
    if !state.compatible {
        return Err(Error::http(409, "OfficeAce 账号模型缓存格式已变化"));
    }
    Ok(state)
}

pub fn stored(credential: &Credentials) -> Result<Value> {
    let value = snapshot()?.value;
    Ok(
        if value["binding"] == binding(credential) && value["catalog"]["models"].is_array() {
            value
        } else {
            Value::Null
        },
    )
}

fn with_probe(mut catalog: Value, credential: &Credentials) -> Result<Value> {
    if let Some(rows) = catalog["models"].as_array_mut() {
        probe::apply(rows, credential)?;
    }
    Ok(catalog)
}

pub fn list(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        // 聚合展示由宿主从各账号快照产生，不向插件开放其它账号的凭据或缓存。
        return Ok(empty());
    }
    let (record, credential) = account::snapshot()?;
    let value = stored(&credential)?;
    let catalog = if value.is_null() {
        empty()
    } else {
        with_probe(value["catalog"].clone(), &credential)?
    };
    account::same_model(&record)?;
    Ok(catalog)
}

pub fn refresh(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        return Ok(json!({ "refreshed": false, "count": 0, "message": "尚未添加 OfficeAce 账号" }));
    }
    let (record, credential) = account::snapshot()?;
    let old = stored(&credential)?;
    let force = params["force"] == true;
    let age =
        sdk::now().saturating_sub(old["catalog"]["meta"]["refreshed_at"].as_i64().unwrap_or(0));
    if !force && old["catalog"]["meta"]["remote"] == true && (0..TTL).contains(&age) {
        // 探测结论变化才重新同步原快照；保留真实抓取时刻，不伪装刚抓取。
        let catalog = with_probe(old["catalog"].clone(), &credential)?;
        let cached = sdk::catalog::cached()?;
        account::same_model(&record)?;
        return Ok(outcome(
            catalog.clone(),
            catalog["models"] != cached["models"],
        ));
    }
    let fetched = fetch(&credential, &old);
    account::same_model(&record)?;
    match fetched {
        Ok(value) => {
            let observed = snapshot()?;
            if observed.value["binding"] == binding(&credential)
                && observed.value["catalog"]["meta"]["refreshed_at"]
                    .as_i64()
                    .unwrap_or(0)
                    > value["catalog"]["meta"]["refreshed_at"]
                        .as_i64()
                        .unwrap_or(0)
            {
                let catalog = with_probe(observed.value["catalog"].clone(), &credential)?;
                account::same_model(&record)?;
                return Ok(outcome(catalog, true));
            }
            account::same_model(&record)?;
            if !state::compare_exchange(
                Scope::Account,
                KEY,
                1,
                observed.revision,
                value.clone(),
                0,
            )? {
                return Err(Error::http(409, "OfficeAce 目录被另一轮刷新，未覆盖新结果"));
            }
            if force {
                probe::request(&credential)?;
            }
            let catalog = with_probe(value["catalog"].clone(), &credential)?;
            account::same_model(&record)?;
            Ok(outcome(catalog, true))
        }
        Err(error) => {
            let catalog = if old.is_null() {
                empty()
            } else {
                with_probe(old["catalog"].clone(), &credential)?
            };
            let mut result = outcome(catalog, false);
            result["message"] = json!(error.message);
            account::same_model(&record)?;
            Ok(result)
        }
    }
}

fn outcome(mut catalog: Value, refreshed: bool) -> Value {
    catalog["refreshed"] = json!(refreshed);
    catalog["count"] = json!(catalog["models"].as_array().map(Vec::len).unwrap_or(0));
    catalog
}

fn fetch(credential: &Credentials, old: &Value) -> Result<Value> {
    let response = client::model(
        "GET",
        "/v2/models",
        credential,
        None,
        json!({ "accept": "application/json" }),
    )?;
    let reply = client::read(response, false)?;
    if !(200..300).contains(&reply.status)
        || reply
            .value
            .get("error")
            .is_some_and(|error| !error.is_null())
        || reply
            .value
            .get("error_code")
            .is_some_and(|code| !matches!(util::number(code), Some(0.0 | 200.0)))
    {
        return Err(super::errors::model_error(
            reply.status,
            &reply.value,
            credential,
        ));
    }
    let mut rows = model_shape::whitelist(model_shape::parse(&reply.value)?);
    let queried_rates = control::Context::acquire().and_then(|mut context| {
        if binding(&context.credential) != binding(credential) {
            return Err(Error::http(
                409,
                "OfficeAce 账号模型凭据已变化，请重新刷新目录",
            ));
        }
        context.rates()
    });
    let (fresh_rates, rate_warning) = match queried_rates {
        Ok(value) => (model_shape::rates(&value), None),
        Err(error) => (Vec::new(), Some(error.message)),
    };
    let rates = if fresh_rates.is_empty() {
        serde_json::from_value(old["rates"].clone()).unwrap_or_default()
    } else {
        fresh_rates
    };
    model_shape::attach(&mut rows, &rates);
    let mut catalog = sdk::catalog::refreshed(rows);
    if let Some(message) = rate_warning {
        catalog["message"] = json!(format!(
            "模型目录已取得；倍率查询未完成，沿用最后读数：{message}"
        ));
    }
    Ok(json!({ "binding": binding(credential), "catalog": catalog, "rates": rates }))
}
