//! 精选目录提供真实协议和请求 schema；全量目录只补模型 ID，不臆造上下文或能力。
use agent2api_wasm_sdk::{self as sdk, json, state::Scope, Error, Result, Value};
use std::collections::HashSet;

use crate::{account, endpoints, images, token, util};

pub fn defaults() -> Vec<Value> {
    let rows: Vec<Value> = serde_json::from_str(include_str!("fallback.json")).unwrap_or_default();
    let mut models = rows.into_iter().map(|row| {
        let mut model = json!({ "id": row["id"], "name": row["id"], "kind": "chat", "protocol": row["protocol"],
            "supportsToolCall": true, "supportsReasoning": true, "supportsImages": row["images"],
            "maxInputTokens": row["context"], "isDefault": row["id"] == "auto", "catalogSource": "official_public_snapshot" });
        if row["output"].as_u64().is_some_and(|count| count > 0) { model["maxOutputTokens"] = row["output"].clone(); }
        model["authentication"] = row["authentication"].clone();
        model
    }).collect::<Vec<_>>();
    models.extend(images::models());
    models
}

pub fn protocol(row: &Value) -> Result<&'static str> {
    let raw = row["protocol"]["type"]
        .as_str()
        .or_else(|| row["protocol"].as_str())
        .unwrap_or("");
    match raw {
        "openai_chat_completions" | "chat" | "" => Ok("chat"),
        "anthropic_messages" | "anthropic" => Ok("anthropic"),
        "openai_responses" | "responses" => Ok("responses"),
        _ => Err(Error::http(502, "科搭模型目录包含尚未接入的协议")),
    }
}

fn rows(body: &Value) -> Result<&Vec<Value>> {
    body["catalog"]["data"]
        .as_array()
        .or_else(|| body["data"].as_array())
        .filter(|rows| rows.len() <= 2000)
        .ok_or_else(|| Error::http(502, "科搭模型目录格式无效或超过限制"))
}

fn parse(body: &Value) -> Result<Vec<Value>> {
    let mut models = Vec::new();
    let mut seen = HashSet::new();
    for row in rows(body)? {
        let Some(id) = row["id"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
        else {
            continue;
        };
        if !seen.insert(id.to_string()) || images::is_model(id) {
            continue;
        }
        let wire = protocol(row)?;
        let name = util::safe_text(&util::text(row, &["display_name", "name"]), &[], 120);
        let mut model = json!({ "id": id, "name": if name.is_empty() { id } else { &name }, "kind": "chat", "protocol": wire,
            "isDefault": id == "auto", "catalogSource": "codar_models" });
        for (target, source) in [
            ("supportsToolCall", "function_tools"),
            ("streaming", "streaming"),
        ] {
            if let Some(value) = row["capabilities"][source].as_bool() {
                model[target] = json!(value);
            }
        }
        if let Some(modalities) = row["input_modalities"].as_array() {
            model["supportsImages"] = json!(modalities.iter().any(|kind| *kind == "image"));
        }
        if let Some(context) = util::amount(&row["context_window"]).filter(|value| *value > 0.0) {
            model["maxInputTokens"] = json!(context as u64);
        }
        let props = &row["request_schema"]["properties"];
        if let Some(output) = ["max_tokens", "max_completion_tokens", "max_output_tokens"]
            .iter()
            .find_map(|key| util::amount(&props[*key]["maximum"]).filter(|value| *value > 0.0))
        {
            model["maxOutputTokens"] = json!(output as u64);
        }
        let levels = row["reasoning"]["levels"]
            .as_array()
            .into_iter()
            .flatten()
            .take(16)
            .filter_map(|row| {
                row["id"]
                    .as_str()
                    .filter(|id| !id.is_empty() && id.len() <= 64)
            })
            .collect::<Vec<_>>();
        if !levels.is_empty() {
            model["reasoningEffortLevels"] = json!(levels);
            model["supportsReasoning"] = json!(true);
            if let Some(default) = row["reasoning"]["default"]
                .as_str()
                .filter(|default| levels.contains(default))
            {
                model["reasoningEffortDefault"] = json!(default);
            }
        }
        if row["reasoning"].is_boolean() {
            model["supportsReasoning"] = row["reasoning"].clone();
        }
        let pricing = &row["pricing"];
        if pricing.is_object() {
            model["pricing"] = pricing.clone();
        }
        model["reasoningRules"] = row["reasoning"].clone();
        model["authentication"] = row["protocol"]["authentication"].clone();
        model["protocolHeaders"] = row["protocol"]["headers"].clone();
        models.push(model);
    }
    if models.is_empty() && !rows(body)?.is_empty() {
        return Err(Error::http(502, "科搭目录无法解析，保留原清单"));
    }
    Ok(models)
}

pub fn fetch(current: &account::Snapshot) -> Result<Vec<Value>> {
    let body = current.request("GET", endpoints::API, endpoints::CATALOG, None, 12_000)?;
    let mut models = parse(&body)?;
    if models.is_empty() {
        // 上游成功的空目录是当前账号的真实结果，不能用全量补单和静态图片重建权限。
        return Ok(models);
    }
    let mut known = models
        .iter()
        .filter_map(|row| row["id"].as_str().map(str::to_string))
        .collect::<HashSet<_>>();
    if let Err(error) = current.request("GET", endpoints::API, endpoints::MODELS, None, 5000).and_then(|body| {
        if let Ok(extra) = rows(&body) {
            for row in extra {
                let Some(id) = row["id"].as_str().filter(|id| {
                    !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
                }) else {
                    continue;
                };
                let lower = id.to_ascii_lowercase();
                if images::is_model(id)
                    || [
                        "embedding",
                        "-image",
                        "nano-banana",
                        "whisper",
                        "tts",
                        "audio",
                    ]
                    .iter()
                    .any(|kind| lower.contains(kind))
                    || !known.insert(id.to_string())
                {
                    continue;
                }
                let anthropic = id.starts_with("claude-");
                models.push(json!({ "id": id, "name": id, "kind": "chat", "protocol": if anthropic { "anthropic" } else { "chat" },
                    "authentication": if anthropic { "x-api-key" } else { "bearer" }, "catalogSource": "all_models" }));
            }
        }
        Ok(())
    }) {
        if matches!(error.code.as_deref(), Some("codar_account_changed" | "codar_identity_mismatch"))
            || error.status == Some(401)
        { return Err(error); }
    }
    current.unchanged()?;
    models.extend(images::models());
    if models.len() > 2000
        || serde_json::to_vec(&models)
            .map_err(|_| Error::http(502, "科搭模型目录无法编码"))?
            .len()
            > 240 * 1024
    {
        return Err(Error::http(502, "科搭模型目录超过缓存限制"));
    }
    Ok(models)
}

pub fn list(params: &Value) -> Result<Value> {
    if params["account"].as_str().is_some_and(|id| !id.is_empty()) {
        let cached = sdk::catalog::cached()?;
        if cached["models"].is_array() {
            return Ok(cached);
        }
    }
    Ok(json!({ "models": defaults(), "meta": { "remote": false, "refreshed_at": 0 } }))
}

pub fn refresh() -> Result<Value> {
    token::ensure(false)?;
    let current = account::current()?;
    let models = fetch(&current)?;
    let old = sdk::state::get(Scope::Account, "catalog-auth", 1)?;
    if !old.compatible
        || !sdk::state::compare_exchange(
            Scope::Account,
            "catalog-auth",
            1,
            old.revision,
            json!({ "fingerprint": util::digest(&current.access()?), "at": util::now()? }),
            0,
        )?
    {
        return Err(Error::http(409, "科搭模型目录的账号状态已变化"));
    }
    current.unchanged()?;
    Ok(sdk::catalog::refreshed(models))
}

pub fn for_model(current: &account::Snapshot, id: &str) -> Result<Value> {
    let cached = sdk::catalog::cached()?;
    let auth = sdk::state::get(Scope::Account, "catalog-auth", 1)?;
    let at = cached["meta"]["refreshed_at"].as_i64().unwrap_or(0);
    // 未手动强刷也要尊重成功保存的空目录，不能再次拉取后假装账号拥有模型。
    if cached["meta"]["remote"] == true && cached["models"].as_array().is_some_and(Vec::is_empty) {
        return Err(Error::http(
            403,
            "科搭当前账号的官方目录为空，请手动刷新目录后再选择模型",
        ));
    }
    let rows = if auth.compatible
        && auth.value["fingerprint"] == util::digest(&current.access()?)
        && cached["meta"]["remote"] == true
        && (0..600_000).contains(&sdk::now().saturating_sub(at))
    {
        cached["models"].as_array().cloned().unwrap_or_default()
    } else {
        fetch(current)?
    };
    rows.into_iter().find(|row| row["id"] == id).ok_or_else(|| {
        Error::http(
            400,
            "科搭当前账号目录中没有该模型，请刷新目录并检查模型名称",
        )
    })
}
