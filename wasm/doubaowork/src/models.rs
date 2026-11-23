//! 目录解析保留原生 mode::model 与模式选择；enabled:false 仅是上游菜单元数据。
//! 内部模式参数留在账号 state，公开目录只投影显示字段，不作为账号硬权限。
//! Copyright (c) 2026 edison7009 and EchoBird contributors. MIT notice: licenses/EchoBird-MIT.txt.

use std::collections::BTreeMap;

use agent2api_wasm_sdk::{json, Result, Value};
use serde::{Deserialize, Serialize};

use crate::{errors, gates, util};

pub const MAX_MODELS: usize = 512;

#[derive(Clone, Deserialize, Serialize, PartialEq)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub extra: Value,
    pub agent_mode: u64,
    pub mode_id: Option<String>,
    pub mode_name: Option<String>,
    pub deep_think: i64,
    pub enabled: Option<bool>,
    #[serde(default)]
    pub reasoning_default: Option<u64>,
    #[serde(default)]
    pub reasoning_levels: Vec<u64>,
    #[serde(default)]
    pub conversation_mode: Option<String>,
}

fn rows<'a>(value: &'a Value, required: bool) -> Result<&'a [Value]> {
    match value {
        Value::Array(rows) if rows.len() <= MAX_MODELS => Ok(rows),
        Value::Null if !required => Ok(&[]),
        _ => Err(errors::protocol(
            "豆包工作模型目录结构不完整或超过大小限制，保留旧清单",
        )),
    }
}

fn safe_id(value: &Value, secrets: &[String]) -> Result<String> {
    util::identifier(value)
        .filter(|id| !util::protected(id, secrets) && !id.contains("://"))
        .ok_or_else(|| errors::protocol("豆包工作模型目录包含无效标识"))
}

fn model_row(row: &Value, mode: Option<&Value>, secrets: &[String]) -> Result<Model> {
    if !row.is_object() {
        return Err(errors::protocol("豆包工作模型目录项不是对象"));
    }
    let id = util::identifier(&row["model_item_key"])
        .or_else(|| util::identifier(&row["item_id"]))
        .ok_or_else(|| errors::protocol("豆包工作模型目录缺少有效模型编号"))?;
    let id = safe_id(&json!(id), secrets)?;
    let name = util::safe_text(util::text(row, &["name"]), secrets, 160);
    let extra = match &row["model_extra_params"] {
        Value::String(raw) if raw.len() <= 16 * 1024 => {
            serde_json::from_str(raw).unwrap_or_else(|_| json!(raw))
        }
        Value::Object(_) => row["model_extra_params"].clone(),
        Value::Null => json!({}),
        _ => return Err(errors::protocol("豆包工作模型附加参数格式无效")),
    };
    if extra.to_string().len() > 16 * 1024 || !util::private_metadata(&extra, secrets, 0) {
        return Err(errors::protocol(
            "豆包工作模型附加参数过大或包含受保护信息，未保存",
        ));
    }
    let agent_mode = match mode {
        Some(mode) => util::unsigned(&mode["agent_mode"])
            .ok_or_else(|| errors::protocol("豆包工作模式缺少 agent_mode"))?,
        None => match row.get("agent_mode") {
            None | Some(Value::Null) => 0,
            Some(value) => {
                util::unsigned(value).ok_or_else(|| errors::protocol("豆包工作模型模式编号无效"))?
            }
        },
    };
    let mode_id = mode
        .and_then(|mode| mode.get("mode_id"))
        .or_else(|| row.get("default_mode"))
        .filter(|value| !value.is_null())
        .map(|value| safe_id(value, secrets))
        .transpose()?;
    let mode_name = mode
        .map(|mode| util::safe_text(util::text(mode, &["name"]), secrets, 100))
        .filter(|name| !name.is_empty());
    let mut deep_think = util::unsigned(&row["menu_type"])
        .or_else(|| util::unsigned(&row["item_id"]))
        .or_else(|| id.parse().ok())
        .unwrap_or(0);
    if mode_id.as_deref() == Some("2") && deep_think == 4 {
        deep_think = 3;
    }
    let reasoning = row
        .get("reasoning_effort_config")
        .filter(|value| !value.is_null());
    let mut reasoning_levels = Vec::new();
    let mut reasoning_default = None;
    if let Some(config) = reasoning {
        if !config.is_object() {
            return Err(errors::protocol("豆包工作思考档位配置无效"));
        }
        for option in rows(&config["option_list"], true)? {
            let level = util::unsigned(&option["level"])
                .filter(|level| (1..=65535).contains(level))
                .ok_or_else(|| errors::protocol("豆包工作思考档位编号无效"))?;
            if !reasoning_levels.contains(&level) {
                reasoning_levels.push(level);
            }
        }
        reasoning_default = config
            .get("default_level")
            .filter(|value| !value.is_null())
            .map(|value| {
                util::unsigned(value)
                    .filter(|level| reasoning_levels.contains(level))
                    .ok_or_else(|| errors::protocol("豆包工作默认思考档位不在官方清单中"))
            })
            .transpose()?;
    }
    let conversation_mode = mode
        .and_then(|mode| mode.get("default_conversation_mode"))
        .filter(|value| !value.is_null() && value.as_str() != Some(""))
        .map(|value| safe_id(value, secrets))
        .transpose()?;
    Ok(Model {
        id: id.clone(),
        name: if name.is_empty() { id } else { name },
        extra,
        agent_mode,
        mode_id,
        mode_name,
        deep_think: i64::try_from(deep_think)
            .map_err(|_| errors::protocol("豆包工作思考标识超出范围"))?,
        enabled: row["enabled"].as_bool(),
        reasoning_default,
        reasoning_levels,
        conversation_mode,
    })
}

fn mode_permits(row: &Value, mode: &Value) -> bool {
    let Some(mode_id) = util::identifier(&mode["mode_id"]) else {
        return false;
    };
    let id = util::identifier(&row["model_item_key"]).or_else(|| util::identifier(&row["item_id"]));
    row["conversation_mode_list"]
        .as_array()
        .map_or(true, |modes| {
            modes
                .iter()
                .any(|value| util::identifier(value).as_ref() == Some(&mode_id))
        })
        && mode["support_models"].as_array().map_or(true, |models| {
            models.iter().any(|value| {
                util::identifier(&value["model_item_key"])
                    .or_else(|| util::identifier(&value["item_id"]))
                    .or_else(|| util::identifier(value))
                    == id
            })
        })
}

fn insert(contexts: &mut BTreeMap<String, Vec<Model>>, model: Model) -> Result<()> {
    let count = contexts.values().map(Vec::len).sum::<usize>();
    let variants = contexts.entry(model.id.clone()).or_default();
    if let Some(previous) = variants.iter_mut().find(|previous| {
        previous.mode_id == model.mode_id && previous.agent_mode == model.agent_mode
    }) {
        *previous = model;
    } else {
        if count >= MAX_MODELS {
            return Err(errors::protocol("豆包工作模型目录条数超过限制"));
        }
        variants.push(model);
    }
    Ok(())
}

pub fn parse(value: &Value, secrets: &[String]) -> Result<BTreeMap<String, Model>> {
    let data = value
        .get("data")
        .ok_or_else(|| errors::protocol("豆包工作模型目录缺少 data"))?;
    let entries = rows(&data["entry_list"], true)?;
    let mut contexts: BTreeMap<String, Vec<Model>> = BTreeMap::new();
    for entry in entries {
        let conf = &entry["active_switch_conf"];
        if !conf.is_object() {
            return Err(errors::protocol("豆包工作目录缺少模式配置"));
        }
        if conf["use_menu_conf_v2"] != true {
            for row in rows(&conf["menu_conf"]["item_list"], true)? {
                insert(&mut contexts, model_row(row, None, secrets)?)?;
            }
            continue;
        }
        let v2 = &conf["menu_conf_v2"];
        if !v2.is_object() {
            return Err(errors::protocol("豆包工作新版目录配置无效"));
        }
        let modes = rows(&v2["mode_list"]["item_list"], false)?;
        if !v2["model_list"]["item_list"].is_array()
            && !modes
                .iter()
                .any(|mode| mode["model_list"]["item_list"].is_array())
        {
            return Err(errors::protocol(
                "豆包工作新版目录缺少模型列表，未把缺失字段当成空目录",
            ));
        }
        for mode in modes {
            safe_id(&mode["mode_id"], secrets)?;
            if util::unsigned(&mode["agent_mode"]).is_none() {
                return Err(errors::protocol("豆包工作模式元数据不完整"));
            }
            rows(&mode["support_models"], false)?;
        }
        for row in rows(&v2["model_list"]["item_list"], false)? {
            rows(&row["conversation_mode_list"], false)?;
            let find = |id: Option<String>| {
                id.and_then(|id| {
                    modes.iter().find(|mode| {
                        util::identifier(&mode["mode_id"]).as_ref() == Some(&id)
                            && mode_permits(row, mode)
                    })
                })
            };
            let mode = find(util::identifier(&row["default_mode"]))
                .or_else(|| {
                    find(util::identifier(
                        &v2["default_select_mode_map"]["new_chat"]["mode_id"],
                    ))
                })
                .or_else(|| {
                    find(util::identifier(
                        &v2["default_select_mode_map"]["office"]["mode_id"],
                    ))
                })
                .or_else(|| {
                    modes.iter().find(|mode| {
                        util::unsigned(&mode["agent_mode"]) == Some(2) && mode_permits(row, mode)
                    })
                })
                .or_else(|| modes.iter().find(|mode| mode_permits(row, mode)));
            if !modes.is_empty() && mode.is_none() {
                continue;
            }
            insert(&mut contexts, model_row(row, mode, secrets)?)?;
        }
        // 同模式下模式内元数据优先，不能拿全局条目丢掉本地参数。
        for mode in modes {
            for row in rows(&mode["model_list"]["item_list"], false)? {
                let mut model = model_row(row, Some(mode), secrets)?;
                if let Some(previous) = contexts.get(&model.id).and_then(|models| {
                    models.iter().find(|previous| {
                        previous.mode_id == model.mode_id && previous.agent_mode == model.agent_mode
                    })
                }) {
                    if model.reasoning_levels.is_empty() {
                        model.reasoning_levels = previous.reasoning_levels.clone();
                        model.reasoning_default = previous.reasoning_default;
                    }
                    if model.conversation_mode.is_none() {
                        model.conversation_mode = previous.conversation_mode.clone();
                    }
                }
                insert(&mut contexts, model)?;
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
                model.name = format!(
                    "{} · {}",
                    model.name,
                    model.mode_name.as_deref().unwrap_or(&mode_id)
                );
                format!("{}::{}", util::encode(&mode_id), util::encode(&id))
            } else {
                id.clone()
            };
            if key.len() > 512 || result.insert(key, model).is_some() {
                return Err(errors::protocol("豆包工作目录模式标识冲突，未覆盖旧映射"));
            }
        }
    }
    Ok(result)
}

pub fn effort_name(level: u64) -> Option<&'static str> {
    match level {
        3 => Some("low"),
        4 | 8 => Some("medium"),
        5 => Some("high"),
        6 => Some("xhigh"),
        7 => Some("max"),
        _ => None,
    }
}

pub fn display(models: &BTreeMap<String, Model>, fetched_at: i64) -> Value {
    let mut rows: Vec<_> = models
        .iter()
        .map(|(key, model)| {
            let mut row = json!({ "id": key, "name": model.name, "kind": "chat",
            "supportsImages": true, "supportsReasoning": model.deep_think != 0 || model.mode_id.is_some(), "supportsToolCall": true,
            "toolCallEmulated": true,
            "chatReady": true, "migrationStage": "complete", "availabilityMessage": gates::SUMMARY,
            "doubaowork": { "modelItemKey": model.id, "agentMode": model.agent_mode,
                "modeId": model.mode_id, "modeName": model.mode_name, "deepThink": model.deep_think,
                "menuEnabled": model.enabled, "permissionKnown": false,
                "nativeCapabilities": { "images": true, "toolCallEmulated": true,
                    "reasoning": model.deep_think != 0 || model.mode_id.is_some() } } });
            if !model.reasoning_levels.is_empty() {
                row["reasoningEffortLevels"] = json!(model.reasoning_levels.iter()
                    .filter_map(|level| effort_name(*level)).collect::<Vec<_>>());
                row["reasoningSource"] = json!("upstream_declared");
            }
            if let Some(context) = model
                .extra
                .get("context_window")
                .or_else(|| model.extra.get("max_input_tokens"))
                .or_else(|| model.extra.get("total_window_size"))
                .and_then(util::unsigned)
                .filter(|value| *value > 0 && *value <= (1u64 << 53) - 1)
            {
                row["maxInputTokens"] = json!(context);
            }
            row
        })
        .collect();
    if models
        .values()
        .any(|model| matches!(model.agent_mode, 1 | 2))
    {
        rows.push(crate::image_generation::model_row());
    }
    json!({ "schema": 1, "provider": "doubaowork", "models": rows,
        "meta": { "remote": true, "refreshed_at": fetched_at }, "displayOnly": true,
        "permissionsKnown": false, "chatReady": true })
}

pub fn empty() -> Value {
    json!({ "models": [], "meta": { "remote": false, "refreshed_at": 0 }, "displayOnly": true,
        "permissionsKnown": false, "chatReady": true, "message": "尚无经过核对的豆包工作目录，请选择账号刷新" })
}
