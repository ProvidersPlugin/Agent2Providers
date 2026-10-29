use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{accounts, api, PROVIDER_ID};
use crate::errors::GatewayError;

const TTL_MS: i64 = 5 * 60 * 1000;
const LEDGER: &str = "doubaowork-models";

#[derive(Clone, Deserialize, Serialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub extra: Value,
    pub agent_mode: u64,
    pub mode_id: Option<String>,
    pub mode_name: Option<String>,
    pub deep_think: i64,
}

#[derive(Clone, Deserialize, Serialize)]
struct Catalog {
    fetched_at: i64,
    credential: String,
    bot: String,
    models: BTreeMap<String, Model>,
}

fn cache() -> &'static Mutex<BTreeMap<String, Catalog>> {
    static CACHE: OnceLock<Mutex<BTreeMap<String, Catalog>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

fn model_row(row: &Value, mode: Option<&Value>) -> Option<Model> {
    let id =
        api::identifier(&row["model_item_key"]).or_else(|| api::identifier(&row["item_id"]))?;
    let name = row["name"]
        .as_str()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&id)
        .to_string();
    let extra = match &row["model_extra_params"] {
        Value::String(text) => {
            serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.clone()))
        }
        Value::Object(_) => row["model_extra_params"].clone(),
        _ => json!({}),
    };
    let agent_mode = match mode {
        Some(mode) => api::number(&mode["agent_mode"])?,
        None => api::number(&row["agent_mode"]).unwrap_or(0),
    };
    let mode_id = mode
        .and_then(|mode| api::identifier(&mode["mode_id"]))
        .or_else(|| api::identifier(&row["default_mode"]));
    let mode_name = mode
        .and_then(|mode| mode["name"].as_str())
        .filter(|name| !name.trim().is_empty())
        .map(str::to_string);
    let mut deep_think = api::number(&row["menu_type"])
        .or_else(|| api::number(&row["item_id"]))
        .or_else(|| id.parse().ok())
        .unwrap_or(0) as i64;
    if mode_id.as_deref() == Some("2") && deep_think == 4 {
        deep_think = 3;
    }
    Some(Model {
        id,
        name,
        extra,
        agent_mode,
        mode_id,
        mode_name,
        deep_think,
    })
}

fn mode_permits_model(row: &Value, mode: &Value) -> bool {
    let Some(mode_id) = api::identifier(&mode["mode_id"]) else {
        return false;
    };
    let id = api::identifier(&row["model_item_key"]).or_else(|| api::identifier(&row["item_id"]));
    row["conversation_mode_list"]
        .as_array()
        .map_or(true, |modes| {
            modes
                .iter()
                .any(|value| api::identifier(value).as_ref() == Some(&mode_id))
        })
        && mode["support_models"].as_array().map_or(true, |models| {
            models.iter().any(|value| {
                api::identifier(&value["model_item_key"])
                    .or_else(|| api::identifier(&value["item_id"]))
                    .or_else(|| api::identifier(value))
                    == id
            })
        })
}

fn directory(value: &Value) -> BTreeMap<String, Model> {
    let mut contexts: BTreeMap<String, Vec<Model>> = BTreeMap::new();
    let mut insert = |model: Model| {
        let variants = contexts.entry(model.id.clone()).or_default();
        if let Some(previous) = variants.iter_mut().find(|previous| {
            previous.mode_id == model.mode_id && previous.agent_mode == model.agent_mode
        }) {
            *previous = model;
        } else {
            variants.push(model);
        }
    };
    for entry in value["data"]["entry_list"].as_array().into_iter().flatten() {
        let conf = &entry["active_switch_conf"];
        if conf["use_menu_conf_v2"] != true {
            for row in conf["menu_conf"]["item_list"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if let Some(model) = model_row(row, None) {
                    insert(model);
                }
            }
            continue;
        }
        let v2 = &conf["menu_conf_v2"];
        let modes = v2["mode_list"]["item_list"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        for row in v2["model_list"]["item_list"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let find = |id: Option<String>| {
                id.and_then(|id| {
                    modes.iter().find(|mode| {
                        api::identifier(&mode["mode_id"]).as_ref() == Some(&id)
                            && mode_permits_model(row, mode)
                    })
                })
            };
            let mode = find(api::identifier(&row["default_mode"]))
                .or_else(|| {
                    find(api::identifier(
                        &v2["default_select_mode_map"]["new_chat"]["mode_id"],
                    ))
                })
                .or_else(|| {
                    modes.iter().find(|mode| {
                        api::number(&mode["agent_mode"]) == Some(2) && mode_permits_model(row, mode)
                    })
                })
                .or_else(|| modes.iter().find(|mode| mode_permits_model(row, mode)));
            if !modes.is_empty() && mode.is_none() {
                continue;
            }
            if let Some(model) = model_row(row, mode) {
                insert(model);
            }
        }
        // 与客户端一致：模式内的元数据优先于同模式的全局条目。
        for mode in modes {
            for row in mode["model_list"]["item_list"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if let Some(model) = model_row(row, Some(mode)) {
                    insert(model);
                }
            }
        }
    }
    let mut result = BTreeMap::new();
    for (id, variants) in contexts {
        let scoped = variants.len() > 1;
        for mut model in variants {
            let key = if scoped {
                let mode_id = model
                    .mode_id
                    .clone()
                    .unwrap_or_else(|| format!("agent-{}", model.agent_mode));
                let mode_name = model.mode_name.as_deref().unwrap_or(&mode_id);
                model.name = format!("{} · {mode_name}", model.name);
                format!("{}::{}", api::encode(&mode_id), api::encode(&id))
            } else {
                id.clone()
            };
            result.insert(key, model);
        }
    }
    result
}

fn model_records(catalog: &Catalog) -> Vec<Value> {
    catalog.models.iter().map(|(id, model)| {
        let mut value = json!({"id":id,"name":model.name,"supportsImages":true,"supportsReasoning":model.deep_think != 0 || model.mode_id.is_some(),"supportsToolCall":true,"toolCallEmulated":true});
        if let Some(context) = model.extra.get("context_window").or_else(||model.extra.get("max_input_tokens")).and_then(Value::as_u64) {
            value["maxInputTokens"] = json!(context);
        }
        value
    }).collect()
}

pub async fn list(ctx: &PluginCtx, _account: &str) -> Result<proto::ModelList, GatewayError> {
    let accounts = ctx.store_list(PROVIDER_ID).await.map_err(|error| {
        GatewayError::with_status(502, format!("读取豆包工作账号失败：{error}"))
    })?;
    let mut models = BTreeMap::new();
    let mut refreshed_at = 0;
    for found in accounts {
        let current = cache()
            .lock()
            .ok()
            .and_then(|cache| cache.get(&found.id).cloned());
        let current = if current.is_some() {
            current
        } else {
            ctx.ledger_get(&found.id, LEDGER)
                .await
                .ok()
                .and_then(|value| serde_json::from_value(value).ok())
        };
        if let Some(current) = current {
            refreshed_at = refreshed_at.max(current.fetched_at);
            for model in model_records(&current) {
                models.insert(model["id"].as_str().unwrap_or("").to_string(), model);
            }
        }
    }
    Ok(proto::ModelList {
        models: models.into_values().collect(),
        meta: proto::CatalogMeta {
            remote: refreshed_at > 0,
            refreshed_at,
        },
    })
}

async fn fetch(
    ctx: &PluginCtx,
    account: &str,
    credentials: &Value,
    client: &reqwest::Client,
    force: bool,
) -> Result<(Catalog, bool), GatewayError> {
    let cookie = accounts::cookie_header(credentials)?;
    let fingerprint: String = Sha256::digest(cookie)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if !force {
        let current = cache().lock().ok().and_then(|cache| {
            cache
                .get(account)
                .filter(|catalog| {
                    catalog.credential == fingerprint
                        && crate::http::now_ms() - catalog.fetched_at < TTL_MS
                })
                .cloned()
        });
        if let Some(current) = current {
            return Ok((current, false));
        }
    }
    let launch = api::post(client, credentials, "/alice/user/launch", &json!({})).await?;
    let bot = api::identifier(&launch["data"]["assistant_bot_id"])
        .or_else(|| api::identifier(&launch["assistant_bot_id"]))
        .ok_or_else(|| GatewayError::with_status(502, "豆包工作未返回助手 ID"))?;
    let value = api::post(
        client,
        credentials,
        "/alice/slot/action_bar_v3/brief_list",
        &json!({"language_code":"zh","bot_id":bot}),
    )
    .await?;
    let models = directory(&value);
    if models.is_empty() {
        return Err(GatewayError::with_status(
            502,
            "豆包工作返回的模型目录为空或格式已变化，保留已有清单",
        ));
    }
    let current = Catalog {
        fetched_at: crate::http::now_ms(),
        credential: fingerprint,
        bot,
        models,
    };
    let value = serde_json::to_value(&current)
        .map_err(|_| GatewayError::with_status(500, "模型目录无法保存"))?;
    ctx.ledger_put(account, LEDGER, &value)
        .await
        .map_err(|error| {
            GatewayError::with_status(502, format!("保存豆包工作目录失败：{error}"))
        })?;
    if let Ok(mut cache) = cache().lock() {
        if cache.len() >= 256 {
            cache.clear();
        }
        cache.insert(account.into(), current.clone());
    }
    Ok((current, true))
}

pub async fn refresh(
    ctx: &PluginCtx,
    account: &str,
    force: bool,
) -> Result<proto::RefreshOutcome, GatewayError> {
    let (id, credentials, client) = api::account(ctx, account, false).await?;
    let (catalog, refreshed) = fetch(ctx, &id, &credentials, &client, force).await?;
    Ok(if refreshed {
        proto::RefreshOutcome::refreshed(catalog.models.len())
    } else {
        proto::RefreshOutcome::unchanged()
    })
}

pub async fn resolve(
    ctx: &PluginCtx,
    account: &str,
    credentials: &Value,
    client: &reqwest::Client,
    name: &str,
) -> Result<(String, Model), GatewayError> {
    let (catalog, _) = fetch(ctx, account, credentials, client, false).await?;
    let selected = catalog
        .models
        .get(name)
        .cloned()
        .or_else(|| {
            let mut matching = catalog.models.values().filter(|model| model.name == name);
            let first = matching.next()?.clone();
            matching.next().is_none().then_some(first)
        })
        .ok_or_else(|| {
            GatewayError::with_status(
                400,
                "该模型不在当前豆包工作账号的在线目录中，请刷新模型或检查模型映射",
            )
            .with_code("model_not_found")
        })?;
    Ok((catalog.bot, selected))
}
