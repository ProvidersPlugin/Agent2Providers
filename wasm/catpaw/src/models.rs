//! 目录归一化的唯一事实来源。成功远程清单优先，静态表只在缺失时兜底。
//! 远程 modelType 与静态不一致时必须以当前远程 ID 为准，不再走静态优先的第二套解析。

use std::collections::{BTreeMap, BTreeSet};

use agent2api_wasm_sdk::{json, Error, Result, Value};

use crate::{errors, util};

const MAX_MODELS: usize = 512;
const MAX_CATALOG_BYTES: usize = 256 * 1024;
const FORMAT: u64 = 1;

fn name_key(name: &str) -> String {
    name.trim().chars().map(|ch| {
        if ch.is_whitespace() || matches!(ch, '_' | '.') { '-' } else { ch.to_ascii_lowercase() }
    }).collect()
}

pub fn fallback() -> Value {
    list(vec![
        json!({ "id": "kimi-k3", "name": "Kimi-K3", "modelType": 83,
            "supportsImages": true, "supportsReasoning": true, "supportsToolCall": true,
            "kind": "chat", "catpawCatalogFormat": FORMAT, "catpawModelTypeSource": "static" }),
        json!({ "id": "glm-5.3-flash", "name": "GLM-5.3-Flash", "modelType": 91,
            "supportsImages": true, "supportsReasoning": true, "supportsToolCall": true, "kind": "chat",
            "maxInputTokens": 1024000, "defaultContextWindow": "1024000",
            "catpawContextWindows": ["204800", "512000", "1024000"],
            "catpawCatalogFormat": FORMAT, "catpawModelTypeSource": "static" }),
    ], false, 0)
}

pub fn list(models: Vec<Value>, remote: bool, at: i64) -> Value {
    json!({ "models": models, "meta": { "remote": remote, "refreshed_at": at } })
}

fn static_entry(id: &str) -> Option<Value> {
    let wanted = name_key(id);
    let source = fallback();
    let models = source["models"].as_array()?;
    models.iter().find(|model| {
        ["id", "name"].iter().any(|key| model[*key].as_str().is_some_and(|text| name_key(text) == wanted))
    }).cloned()
}

fn visible_text(value: Option<&Value>, maximum: usize, secrets: &[&str]) -> Result<Option<String>> {
    let Some(value) = value.filter(|value| !value.is_null()) else { return Ok(None); };
    let text = value.as_str().ok_or_else(|| errors::protocol("CatPaw 目录名称必须是文本"))?.trim();
    if text.is_empty() { return Ok(None); }
    if text.chars().count() > maximum || text.chars().any(char::is_control)
        || text.contains("://") || errors::contains_secret(text, secrets) {
        return Err(errors::protocol("CatPaw 目录包含无效或受保护文本，已保留旧清单"));
    }
    Ok(Some(text.to_string()))
}

pub fn parse(payload: &Value, at: i64, secrets: &[&str]) -> Result<Value> {
    let items = payload.as_array().ok_or_else(|| errors::protocol("CatPaw 模型目录不是数组"))?;
    if items.len() > MAX_MODELS {
        return Err(errors::protocol("CatPaw 模型目录超过条目上限，已保留旧清单"));
    }
    if items.is_empty() { return Ok(list(Vec::new(), true, at)); }
    let mut models = BTreeMap::<String, Value>::new();
    for item in items {
        if !item.is_object() { return Err(errors::protocol("CatPaw 目录条目不是对象")); }
        if util::text(item, &["provider"]).eq_ignore_ascii_case("USER_CUSTOM") { continue; }
        let extended = &item["extendedInfo"];
        if let Some(auto) = extended.get("isAuto").filter(|value| !value.is_null()) {
            match util::flag(auto) {
                Some(true) => continue,
                Some(false) => {},
                None => return Err(errors::protocol("CatPaw 目录 auto 标记未知，未猜测真实模型")),
            }
        }
        let Some(id) = visible_text(item.get("modelTypeName"), 256, secrets)? else { continue; };
        let spec = static_entry(&id);
        let (model_type, source) = match item.get("modelTypeId").filter(|value| !value.is_null()) {
            Some(value) => (value.as_i64().filter(|id| *id > 0)
                .ok_or_else(|| errors::protocol("CatPaw 目录的数字模型 ID 无效"))?, "remote"),
            None => match spec.as_ref().and_then(|spec| spec["modelType"].as_i64()) {
                Some(id) => (id, "static_fallback"),
                None => continue, // 无数字 ID 无法转发，不广告占位名称。
            },
        };
        // 名字相同、数字已变时不可把旧档位与新 ID 混成一条模型。
        let same_static = spec.as_ref().filter(|spec| spec["modelType"].as_i64() == Some(model_type));
        let caption = visible_text(extended.get("modelCaptionZhCN"), 256, secrets)?;
        let name = match caption {
            Some(caption) => caption,
            None => visible_text(item.get("description"), 256, secrets)?.unwrap_or_else(|| id.clone()),
        };
        let mut model = json!({ "id": id, "name": name, "modelType": model_type,
            "supportsToolCall": true, "kind": "chat", "catpawCatalogFormat": FORMAT,
            "catpawModelTypeSource": source });
        let params = definitions(item)?;
        add_capability(&mut model, item, "supportImage", "supportsImages", same_static, false)?;
        let effort_evidence = params.iter().any(|entry| entry["id"] == "effort");
        add_capability(&mut model, item, "supportThinking", "supportsReasoning", same_static, effort_evidence)?;
        if let Some(rate) = rate(extended.get("rateMultiplier"))? {
            if errors::contains_secret(&rate, secrets) { return Err(errors::protocol("CatPaw 倍率字段包含受保护内容")); }
            model["credits"] = json!(rate);
        }
        add_context(&mut model, params, same_static)?;
        add_effort(&mut model, params, secrets)?;
        let key = name_key(&id);
        if let Some(previous) = models.get(&key) {
            if previous != &model {
                return Err(Error::http(502, "CatPaw 目录包含重名但资料不同的模型，未随意选择或覆盖旧清单")
                    .with_code("catpaw_catalog_conflict"));
            }
        } else { models.insert(key, model); }
    }
    // 合法响应被过滤为零个官方模型也是本次真实目录，不能保留已撤销的旧模型。
    let result = list(models.into_values().collect(), true, at);
    if result.to_string().len() > MAX_CATALOG_BYTES {
        return Err(errors::protocol("CatPaw 归一化目录超过 256 KiB 上限，已保留旧清单"));
    }
    Ok(result)
}

fn definitions(item: &Value) -> Result<&[Value]> {
    match item.get("parameterDefinitions").filter(|value| !value.is_null()) {
        Some(Value::Array(items)) if items.len() <= 64 && items.iter().all(Value::is_object) => Ok(items),
        None => Ok(&[]),
        _ => Err(errors::protocol("CatPaw 模型参数定义无效或过多")),
    }
}

fn add_capability(model: &mut Value, item: &Value, upstream: &str, field: &str,
    spec: Option<&Value>, inferred: bool) -> Result<()> {
    let flag = match item.get(upstream).filter(|value| !value.is_null()) {
        Some(value) => Some(util::flag(value).ok_or_else(|| errors::protocol("CatPaw 模型能力标记未知"))?),
        None if inferred => Some(true),
        None => spec.and_then(|spec| spec[field].as_bool()),
    };
    if let Some(flag) = flag { model[field] = json!(flag); }
    Ok(())
}

fn rate(value: Option<&Value>) -> Result<Option<String>> {
    let Some(value) = value.filter(|value| !value.is_null()) else { return Ok(None); };
    let text = util::scalar(Some(value)).ok_or_else(|| errors::protocol("CatPaw 模型倍率格式无效"))?;
    let text = text.trim();
    if text.is_empty() { return Ok(None); }
    if text.len() > 64 || util::number(Some(value)).is_none_or(|number| number < 0.0) {
        return Err(errors::protocol("CatPaw 模型倍率不是有效非负数"));
    }
    Ok(Some(format!("x{text} credits")))
}

fn definition<'a>(definitions: &'a [Value], id: &str) -> Result<Option<&'a Value>> {
    let mut found = definitions.iter().filter(|definition| definition["id"] == id);
    let first = found.next();
    if found.next().is_some() { return Err(errors::protocol("CatPaw 模型含重复参数定义")); }
    Ok(first)
}

fn add_context(model: &mut Value, definitions: &[Value], spec: Option<&Value>) -> Result<()> {
    let Some(context) = definition(definitions, "context")? else {
        if let Some(spec) = spec {
            for field in ["maxInputTokens", "defaultContextWindow", "catpawContextWindows"] {
                if let Some(value) = spec.get(field) { model[field] = value.clone(); }
            }
        }
        return Ok(());
    };
    let values = context["values"].as_array().filter(|values| !values.is_empty() && values.len() <= 64)
        .ok_or_else(|| errors::protocol("CatPaw 上下文档位列表无效"))?;
    let mut windows = BTreeSet::new();
    for row in values {
        let window = util::integer(&row["value"]).filter(|window| *window > 0)
            .ok_or_else(|| errors::protocol("CatPaw 上下文档位不是有效正整数"))?;
        windows.insert(window);
    }
    if let Some(maximum) = windows.last() { model["maxInputTokens"] = json!(maximum); }
    model["catpawContextWindows"] = json!(windows.iter().map(i64::to_string).collect::<Vec<_>>());
    if let Some(value) = context.get("defaultValue").filter(|value| !value.is_null()) {
        let default = util::integer(value).filter(|value| windows.contains(value))
            .ok_or_else(|| errors::protocol("CatPaw 默认上下文档位不在当前目录中"))?;
        model["defaultContextWindow"] = json!(default.to_string());
    }
    Ok(())
}

fn add_effort(model: &mut Value, definitions: &[Value], secrets: &[&str]) -> Result<()> {
    let Some(effort) = definition(definitions, "effort")? else { return Ok(()); };
    let Some(values) = effort.get("values").filter(|value| !value.is_null()) else { return Ok(()); };
    let values = values.as_array().filter(|values| values.len() <= 64)
        .ok_or_else(|| errors::protocol("CatPaw 思考档位列表无效"))?;
    let mut levels = Vec::new();
    for row in values {
        let level = visible_text(row.get("value"), 64, secrets)?
            .ok_or_else(|| errors::protocol("CatPaw 思考档位缺少有效值"))?;
        if !levels.contains(&level) { levels.push(level); }
    }
    if !levels.is_empty() { model["catpawEfforts"] = json!(levels); }
    Ok(())
}

/// 宿主 catalog.get 可能恢复原生旧目录；只投影本家认识的公开字段，丢弃无证据键。
pub fn cached(value: &Value) -> Result<Option<Value>> {
    if value.is_null() { return Ok(None); }
    let Some(rows) = value["models"].as_array() else { return Ok(None); };
    let remote = value["meta"]["remote"] == true;
    let at = value["meta"]["refreshed_at"].as_i64().unwrap_or(0);
    if remote && at <= 0 { return Err(errors::protocol("CatPaw 远程目录缓存缺少真实刷新时间")); }
    if rows.is_empty() { return Ok(remote.then(|| list(Vec::new(), true, at))); }
    if rows.len() > MAX_MODELS || value.to_string().len() > MAX_CATALOG_BYTES { return Err(errors::protocol("CatPaw 缓存目录超过大小限制")); }
    let mut saved = BTreeMap::<String, Value>::new();
    for row in rows {
        if !row.is_object() { return Err(errors::protocol("CatPaw 缓存目录条目无效")); }
        let id = visible_text(row.get("id"), 256, &[])?.ok_or_else(|| errors::protocol("CatPaw 缓存模型缺少名称"))?;
        if row.get("catpawCatalogFormat").is_some_and(|value| value.as_u64() != Some(FORMAT)) {
            return Err(errors::protocol("CatPaw 缓存目录格式已变化，请重新获取"));
        }
        let spec = static_entry(&id);
        let from_remote_id = row.get("modelType").is_some_and(|value| !value.is_null());
        let model_type = match row.get("modelType").filter(|value| !value.is_null()) {
            Some(value) => value.as_i64().filter(|id| *id > 0).ok_or_else(|| errors::protocol("CatPaw 缓存数字模型 ID 无效"))?,
            None => match spec.as_ref().and_then(|spec| spec["modelType"].as_i64()) { Some(id) => id, None => continue },
        };
        let name = visible_text(row.get("name"), 256, &[])?.unwrap_or_else(|| id.clone());
        if remote && row.get("catpawCatalogFormat").is_none()
            && spec.as_ref().is_some_and(|spec| spec["modelType"].as_i64() != Some(model_type)) {
            return Err(Error::http(502, "CatPaw 旧缓存的远程模型 ID 与静态资料冲突，请重新获取当前目录")
                .with_code("catpaw_catalog_conflict"));
        }
        let known_source = row["catpawModelTypeSource"].as_str().filter(|source|
            matches!(*source, "remote" | "remote_cache" | "static" | "static_fallback"));
        let source = known_source.unwrap_or(if remote && from_remote_id { "remote_cache" }
            else if remote { "static_fallback" } else { "static" });
        let mut model = json!({ "id": id, "name": name, "modelType": model_type, "kind": "chat",
            "catpawCatalogFormat": FORMAT, "catpawModelTypeSource": source });
        for field in ["supportsImages", "supportsReasoning", "supportsToolCall"] {
            if let Some(value) = row.get(field).filter(|value| !value.is_null()) {
                model[field] = json!(util::flag(value).ok_or_else(|| errors::protocol("CatPaw 缓存能力标记无效"))?);
            }
        }
        for field in ["maxInputTokens", "maxOutputTokens"] {
            if let Some(value) = row.get(field).filter(|value| !value.is_null()) {
                model[field] = json!(util::integer(value).filter(|number| *number > 0)
                    .ok_or_else(|| errors::protocol("CatPaw 缓存上下文读数无效"))?);
            }
        }
        if let Some(default) = visible_text(row.get("defaultContextWindow"), 64, &[])? {
            if default.parse::<i64>().ok().filter(|value| *value > 0).is_none() { return Err(errors::protocol("CatPaw 缓存默认档位无效")); }
            model["defaultContextWindow"] = json!(default);
        }
        if let Some(credits) = visible_text(row.get("credits"), 96, &[])? {
            let number = credits.strip_prefix('x').and_then(|value| value.strip_suffix(" credits"))
                .and_then(|text| text.parse::<f64>().ok()).filter(|value| value.is_finite() && *value >= 0.0);
            if number.is_none() { return Err(errors::protocol("CatPaw 缓存倍率不是有效读数")); }
            model["credits"] = json!(credits);
        }
        for field in ["catpawContextWindows", "catpawEfforts"] {
            if let Some(values) = row.get(field) {
                let values = values.as_array().filter(|values| values.len() <= 64)
                    .ok_or_else(|| errors::protocol("CatPaw 缓存模型档位无效"))?;
                let values = values.iter().map(|value| visible_text(Some(value), 64, &[])
                    .and_then(|value| value.ok_or_else(|| errors::protocol("CatPaw 缓存档位为空"))))
                    .collect::<Result<Vec<_>>>()?;
                model[field] = json!(values);
            }
        }
        let key = name_key(&id);
        if saved.get(&key).is_some_and(|previous| previous != &model) {
            return Err(Error::http(502, "CatPaw 缓存存在冲突的同名模型，需重新获取目录").with_code("catpaw_catalog_conflict"));
        }
        saved.insert(key, model);
    }
    if saved.is_empty() {
        return Err(errors::protocol("CatPaw 缓存目录没有可用数字模型，未复活另一份静态映射"));
    }
    Ok(Some(list(saved.into_values().collect(), remote, at)))
}

// ── 入站纯解析：只从调用方本次给出的展示目录选模型，不读账号、不刷新目录 ──

/// 构造入口只在本模块；没有 Deserialize，不能直接从 Chat 自带 modelType 构造。
/// 这是目录里的协议参数，不是账号可使用该模型的授权或远端执行许可。
pub(super) struct Selection {
    requested: String,
    entry: Value,
}

pub(super) struct RequestModel {
    pub(super) model_type: i64,
    pub(super) display_name: String,
    pub(super) effort: Option<String>,
    pub(super) context: Option<String>,
}

/// 调用方传 catalog::list() 返回的同一份清单；不另查静态表补失踪模型。
/// 纯数字也必须命中该清单，不把未经目录确认的数字直接发给上游。
pub(super) fn select(displayed: &Value, requested: &Value) -> Result<Selection> {
    let requested = requested_model(requested)?;
    selection_budget(displayed)?;
    let normalized = cached(displayed)?
        .ok_or_else(|| errors::invalid("CatPaw 当前没有可解析的模型目录，未选择默认模型"))?;
    let rows = normalized["models"].as_array()
        .ok_or_else(|| errors::protocol("CatPaw 目录缺少模型数组"))?;
    // 规范 id 优先于展示名；两条不同 id 的展示名相同不能随意取第一条。
    let key = name_key(&requested);
    let canonical = unique(rows.iter().filter(|row| row["id"].as_str()
        .is_some_and(|id| name_key(id) == key)))?;
    let selected = match canonical {
        Some(row) => row,
        None => {
            let numeric = requested.parse::<i64>().ok().filter(|id| *id > 0);
            unique(rows.iter().filter(|row| numeric.is_some_and(|id| row["modelType"].as_i64() == Some(id))
                || row["name"].as_str().is_some_and(|name| name_key(name) == key)))?
                .ok_or_else(|| errors::invalid("CatPaw 请求模型未在当前目录中，未回落默认或旧静态模型"))?
        },
    };
    Ok(Selection { requested, entry: selected.clone() })
}

fn selection_budget(displayed: &Value) -> Result<()> {
    fn walk(value: &Value, depth: usize, nodes: &mut usize) -> Result<()> {
        *nodes += 1;
        if depth > 32 || *nodes > 32_768 {
            return Err(errors::protocol("CatPaw 模型选择目录的深度或节点数超过限制"));
        }
        match value {
            Value::Array(items) => { for item in items { walk(item, depth + 1, nodes)?; } },
            Value::Object(object) => { for item in object.values() { walk(item, depth + 1, nodes)?; } },
            _ => {},
        }
        Ok(())
    }
    let mut nodes = 0;
    walk(displayed, 0, &mut nodes)?;
    crate::prepare::json_bytes(displayed, MAX_CATALOG_BYTES)?;
    Ok(())
}

fn unique<'a>(mut rows: impl Iterator<Item = &'a Value>) -> Result<Option<&'a Value>> {
    let first = rows.next();
    if rows.next().is_some() {
        return Err(errors::invalid("CatPaw 模型名称或数字 ID 在当前目录中有歧义，请使用明确的规范模型名"));
    }
    Ok(first)
}

fn requested_model(value: &Value) -> Result<String> {
    let value = match value {
        Value::String(text) => text.trim().to_string(),
        Value::Number(number) => number.as_i64().filter(|id| *id > 0)
            .map(|id| id.to_string()).ok_or_else(|| errors::invalid("CatPaw 数字模型 ID 必须是正整数"))?,
        _ => return Err(errors::invalid("CatPaw 必须明确指定模型，未选择默认模型")),
    };
    if value.is_empty() || value.len() > 1024 || value.chars().any(char::is_control) {
        return Err(errors::invalid("CatPaw 模型名称为空、过长或含控制字符"));
    }
    Ok(value)
}

impl Selection {
    pub(super) fn check_images(&self) -> Result<()> {
        if self.entry["supportsImages"] != true {
            return Err(errors::invalid("CatPaw 当前模型目录未声明图片输入支持，请更换模型"));
        }
        Ok(())
    }

    pub(super) fn check_tools(&self, has_tools: bool) -> Result<()> {
        if has_tools && self.entry["supportsToolCall"] == false {
            return Err(errors::invalid("CatPaw 当前目录模型明确不支持工具调用"));
        }
        Ok(())
    }

    /// 防止目录选择与另一个 Chat 请求自由拼接；仍不代表账号授权。
    pub(super) fn request(&self, body: &Value) -> Result<RequestModel> {
        if requested_model(&body["model"])? != self.requested {
            return Err(errors::invalid("CatPaw 已解析模型与当前请求不一致"));
        }
        let model_type = self.entry["modelType"].as_i64().filter(|id| *id > 0)
            .ok_or_else(|| errors::protocol("CatPaw 目录模型缺少有效数字 ID"))?;
        let display_name = self.entry["id"].as_str()
            .ok_or_else(|| errors::protocol("CatPaw 目录模型缺少规范名称"))?.to_string();
        Ok(RequestModel { model_type, display_name,
            effort: request_effort(body, &self.entry)?, context: request_context(body, &self.entry)? })
    }
}

/// 只恢复旧版的通用思考档位绑定，不修改客户端本次显式选择；发送仍由原账号目录核对。
pub(super) fn reasoning_patch(params: &Value) -> Value {
    if ["reasoning_effort", "reasoningEffort", "effort"].iter()
        .any(|field| params["body"].get(*field).is_some_and(|value| !value.is_null())) {
        return json!({"action": "skip", "reason": "客户端已指定思考档位，绑定不覆盖"});
    }
    let effort = match params["level"].as_str().unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "minimal" | "low" => "low",
        "medium" | "high" => "high",
        "xhigh" | "max" => "max",
        _ => return json!({"action": "skip", "reason": "CatPaw 未确认该绑定等级的映射，未猜测注入"}),
    };
    json!({"action": "set", "field": "reasoning_effort", "value": effort})
}

fn request_effort(body: &Value, entry: &Value) -> Result<Option<String>> {
    let mut requested: Option<String> = None;
    for field in ["reasoning_effort", "reasoningEffort", "effort"] {
        let Some(value) = body.get(field).filter(|value| !value.is_null()) else { continue; };
        let value = value.as_str().filter(|value| value.len() <= 64)
            .ok_or_else(|| errors::invalid("CatPaw 思考档位必须是有界文本"))?.trim().to_ascii_lowercase();
        if value.is_empty() { return Err(errors::invalid("CatPaw 思考档位不能为空")); }
        if requested.as_ref().is_some_and(|previous| previous != &value) {
            return Err(errors::invalid("CatPaw 多个思考档位字段互相冲突"));
        }
        requested = Some(value);
    }
    let Some(requested) = requested else { return Ok(None); };
    if entry["supportsReasoning"] == false {
        return Err(errors::invalid("CatPaw 当前目录模型不支持所请求的思考档位"));
    }
    if let Some(values) = entry["catpawEfforts"].as_array().filter(|values| !values.is_empty()) {
        let mut matches = values.iter().filter_map(Value::as_str)
            .filter(|value| value.eq_ignore_ascii_case(&requested));
        let selected = matches.next()
            .ok_or_else(|| errors::invalid("CatPaw 思考档位未在当前模型目录中声明"))?;
        if matches.next().is_some() { return Err(errors::invalid("CatPaw 当前目录思考档位存在歧义")); }
        return Ok(Some(selected.to_string()));
    }
    // 只对仍是同一数字 ID 的静态事实补旧协议三档；不把旧档位接到变号的远程模型。
    let known = entry["id"].as_str().and_then(static_entry)
        .is_some_and(|known| known["modelType"] == entry["modelType"]);
    if known && matches!(requested.as_str(), "low" | "high" | "max") { return Ok(Some(requested)); }
    Err(errors::invalid("CatPaw 当前目录没有该模型所请求档位的可靠映射，未猜测发送"))
}

fn context_value(value: &Value) -> Result<String> {
    let text = match value {
        Value::String(text) if text.len() <= 64 => text.trim().to_ascii_lowercase(),
        Value::Number(number) => number.as_i64().filter(|value| *value > 0)
            .map(|value| value.to_string()).ok_or_else(|| errors::invalid("CatPaw 上下文档位必须是正整数"))?,
        _ => return Err(errors::invalid("CatPaw 上下文档位必须是有界文本或正整数")),
    };
    let known = match text.as_str() {
        "200k" => "204800",
        "500k" => "512000",
        "1m" | "1024k" => "1024000",
        value => value,
    };
    known.parse::<i64>().ok().filter(|value| *value > 0).map(|value| value.to_string())
        .ok_or_else(|| errors::invalid("CatPaw 上下文档位不是可识别的正整数或别名"))
}

fn request_context(body: &Value, entry: &Value) -> Result<Option<String>> {
    let mut requested: Option<String> = None;
    for path in ["/context_window", "/contextWindow", "/context_length", "/contextLength",
        "/model_params/context", "/modelParams/context",
        "/request_context/modelParams/declarativeParams/context",
        "/requestContext/modelParams/declarativeParams/context"] {
        let Some(value) = body.pointer(path).filter(|value| !value.is_null()) else { continue; };
        let value = context_value(value)?;
        if requested.as_ref().is_some_and(|previous| previous != &value) {
            return Err(errors::invalid("CatPaw 多个上下文档位字段互相冲突"));
        }
        requested = Some(value);
    }
    let requested = match requested {
        Some(value) => Some(value),
        None => entry.get("defaultContextWindow").map(context_value).transpose()?,
    };
    let Some(requested) = requested else { return Ok(None); };
    let supported = entry["catpawContextWindows"].as_array().is_some_and(|values|
        values.iter().any(|value| value.as_str() == Some(requested.as_str())));
    if !supported { return Err(errors::invalid("CatPaw 上下文档位未在当前模型目录中声明")); }
    Ok(Some(requested))
}
