//! 官方公开客户端配置按国际 Coding / Start 通道刷新；账号快照由宿主保存。
//! 配置只提供模型事实，既不授予访问资格，也不能自选网络来源。
use super::{account, config, network};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    AccountLock, Error, Result, Value,
};
use regex::RegexBuilder;

const PUBLIC_CONFIG: &str =
    "https://raw.githubusercontent.com/zai-org/ZCode/main/config/provider/zcode-builtin.json";
const TTL: i64 = 600_000;
const MODELS: [(&str, &str, i64); 11] = [
    ("glm-5.3", "GLM-5.3", 128_000),
    ("glm-5.3-flash", "GLM-5.3 Flash", 128_000),
    ("glm-5.2", "GLM-5.2", 128_000),
    ("glm-5.1", "GLM-5.1", 64_000),
    ("glm-5", "GLM-5", 64_000),
    ("glm-5-turbo", "GLM-5 Turbo", 64_000),
    ("glm-4.7", "GLM-4.7", 131_072),
    ("glm-4.6", "GLM-4.6", 131_072),
    ("glm-4.5-air", "GLM-4.5 Air", 98_304),
    ("glm-4.6v", "GLM-4.6V", 32_768),
    ("glm-5v-turbo", "GLM-5V Turbo", 131_072),
];

pub fn list() -> Vec<Value> {
    MODELS
        .iter()
        .map(|(id, name, _)| {
            let mut model = json!({ "id": id, "name": name, "supportsToolCall": true });
            if matches!(*id, "glm-4.6v" | "glm-5v-turbo") {
                model["supportsReasoning"] = json!(false);
            }
            match *id {
                "glm-5.3" | "glm-5.3-flash" => {
                    model["supportsVideo"] = json!(*id == "glm-5.3-flash");
                    model["reasoningLevels"] = json!(["low", "high", "max"]);
                    model["reasoningDefaultLevel"] = json!("max");
                }
                "glm-5.2" => {
                    model["reasoningLevels"] = json!(["minimal", "high", "max"]);
                    model["reasoningDefaultLevel"] = json!("max");
                }
                _ => {}
            }
            model
        })
        .collect()
}

pub fn max_output(model: &str) -> Option<i64> {
    MODELS
        .iter()
        .find(|(id, _, _)| id.eq_ignore_ascii_case(model.trim()))
        .map(|(_, _, max)| *max)
}

fn binding(record: &Value) -> String {
    network::hash(&record["secret"].to_string())
}
fn valid_cache(value: &Value, record: &Value, version: &str) -> bool {
    value["models"].is_array()
        && value["meta"]["remote"] == true
        && value["meta"]["binding"] == binding(record)
        && value["meta"]["version"] == version
}

pub fn snapshot() -> Result<Value> {
    let record = account::record()?;
    let cached = state::get(Scope::Account, "international-models", 1)?;
    let version = config::app_version()?;
    account::unchanged(&record)?;
    if cached.compatible && valid_cache(&cached.value, &record, &version) {
        return Ok(cached.value);
    }
    let models = list()
        .into_iter()
        .filter(|model| {
            account::plan(&record["secret"]) != account::START
                || matches!(
                    model["id"].as_str(),
                    Some("glm-5.3-flash" | "glm-5.2" | "glm-5-turbo")
                )
        })
        .collect::<Vec<_>>();
    Ok(json!({ "models": models, "meta": { "remote": false, "refreshed_at": 0 } }))
}

pub fn refresh(force: bool) -> Result<Value> {
    let record = account::record()?;
    let version = config::app_version()?;
    let cached = state::get(Scope::Account, "international-models", 1)?;
    if !force
        && cached.compatible
        && valid_cache(&cached.value, &record, &version)
        && cached.value["meta"]["refreshed_at"]
            .as_i64()
            .is_some_and(|at| at > 0 && at <= sdk::now() && sdk::now() - at < TTL)
    {
        account::unchanged(&record)?;
        return Ok(sdk::catalog::unchanged(
            cached.value["models"].as_array().map_or(0, Vec::len),
        ));
    }
    let reply = network::request(
        "GET",
        PUBLIC_CONFIG,
        json!({ "accept": "application/json" }),
        None,
    )?;
    if !(200..300).contains(&reply.status) {
        return Err(Error::http(
            502,
            "ZCode 国际版官方模型配置读取失败，保留原清单",
        ));
    }
    let provider = if account::plan(&record["secret"]) == account::START {
        "account:zai-start-plan"
    } else {
        "account:zai-individual-coding-plan"
    };
    let models = parse(&reply.value, provider)?;
    account::unchanged(&record)?;
    if config::app_version()? != version {
        return Err(Error::http(409, "ZCode 国际版配置已变化，请重新刷新目录"));
    }
    let mut result = sdk::catalog::refreshed(models);
    result["meta"]["binding"] = json!(binding(&record));
    result["meta"]["version"] = json!(version);
    result["meta"]["source"] = json!(PUBLIC_CONFIG);
    result["meta"]["providerRule"] = json!(provider);
    let now = sdk::now();
    let _lock = AccountLock::acquire()?;
    account::unchanged(&record)?;
    let before = state::get(Scope::Account, "international-models", 1)?;
    if !state::compare_exchange(
        Scope::Account,
        "international-models",
        1,
        before.revision,
        result.clone(),
        TTL,
    )? {
        return Err(Error::http(409, "国际模型目录正在更新，请重试"));
    }
    drop(_lock);
    result["meta"]["refreshed_at"] = json!(now);
    Ok(result)
}

pub fn selected(model: &str) -> Option<Value> {
    let record = account::record().ok()?;
    let version = config::app_version().ok()?;
    let local = state::get(Scope::Account, "international-models", 1).ok()?;
    if local.compatible && valid_cache(&local.value, &record, &version) {
        if let Some(model) = local.value["models"].as_array()?.iter().find(|value| {
            value["id"]
                .as_str()
                .is_some_and(|id| id.eq_ignore_ascii_case(model.trim()))
        }) {
            return Some(model.clone());
        }
    }
    let cached = sdk::catalog::cached().ok()?;
    if valid_cache(&cached, &record, &version) {
        cached["models"]
            .as_array()?
            .iter()
            .find(|value| {
                value["id"]
                    .as_str()
                    .is_some_and(|id| id.eq_ignore_ascii_case(model.trim()))
            })
            .cloned()
    } else {
        None
    }
}

fn rules<'a>(value: &'a Value, key: &str) -> Result<&'a [Value]> {
    let Some(value) = value.get(key) else {
        return Ok(&[]);
    };
    value
        .as_array()
        .filter(|items| items.len() <= 1024)
        .map(Vec::as_slice)
        .ok_or_else(|| Error::http(502, "ZCode 官方模型规则不是有限数组"))
}
fn id(value: &Value) -> Result<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|id| !id.is_empty() && id.len() <= 512 && !id.chars().any(char::is_control))
        .ok_or_else(|| Error::http(502, "ZCode 官方模型标识格式无效"))
}
fn add(ids: &mut Vec<String>, value: &Value) -> Result<()> {
    let id = id(value)?;
    if !ids.iter().any(|existing| existing.eq_ignore_ascii_case(id)) {
        ids.push(id.to_string());
    }
    if ids.len() > 256 {
        return Err(Error::http(502, "ZCode 国际版模型清单超过数量限制"));
    }
    Ok(())
}

fn parse(value: &Value, provider: &str) -> Result<Vec<Value>> {
    let config = value.get("config").unwrap_or(value);
    let provider_rules = rules(&config["providerConfigRules"], "providerRules")?;
    let model_config = &config["modelConfigRules"];
    let specific = rules(model_config, "builtinProviderModelRules")?;
    let global = rules(model_config, "modelRules")?;
    if global.len() > 256 {
        return Err(Error::http(502, "ZCode 官方模型属性规则过多"));
    }
    let mut ids = Vec::new();
    let mut recognized = false;
    for rule in provider_rules
        .iter()
        .filter(|rule| rule["providerId"] == provider)
    {
        recognized = true;
        let values = rule
            .get("builtinModelIds")
            .or_else(|| rule["config"].get("builtinModelIds"))
            .ok_or_else(|| Error::http(502, "国际版模型规则缺少本通道清单"))?;
        let values = values
            .as_array()
            .filter(|values| values.len() <= 256)
            .ok_or_else(|| Error::http(502, "国际版模型规则清单格式无效"))?;
        for value in values {
            add(&mut ids, value)?;
        }
    }
    let specific = specific
        .iter()
        .filter(|rule| rule["providerId"] == provider)
        .collect::<Vec<_>>();
    for rule in &specific {
        recognized = true;
        add(&mut ids, &rule["modelId"])?;
    }
    if !recognized {
        return Err(Error::http(502, "官方配置未包含国际版本通道，保留原目录"));
    }
    let mut compiled = Vec::new();
    for rule in global {
        let pattern = rule["modelMatch"]
            .as_str()
            .filter(|text| !text.is_empty() && text.len() <= 1024)
            .ok_or_else(|| Error::http(502, "ZCode 官方模型匹配规则无效"))?;
        let regex = RegexBuilder::new(&format!("^(?:{pattern})$"))
            .case_insensitive(true)
            .size_limit(1024 * 1024)
            .dfa_size_limit(1024 * 1024)
            .build()
            .map_err(|_| Error::http(502, "ZCode 官方模型匹配规则不可解析"))?;
        compiled.push((regex, &rule["config"]));
    }
    let mut models = Vec::new();
    for model_id in ids {
        let mut model = json!({ "id": model_id.to_ascii_lowercase(), "name": model_id });
        let mut enabled = true;
        for (pattern, options) in &compiled {
            if pattern.is_match(&model_id) {
                apply(&mut model, &mut enabled, options)?;
            }
        }
        for rule in specific.iter().filter(|rule| {
            rule["modelId"]
                .as_str()
                .is_some_and(|id| id.eq_ignore_ascii_case(&model_id))
        }) {
            apply(&mut model, &mut enabled, &rule["config"])?;
        }
        if enabled {
            models.push(model);
        }
    }
    if json!(models).to_string().len() > 220 * 1024 {
        return Err(Error::http(502, "国际模型清单超过大小限制"));
    }
    Ok(models)
}

fn apply(model: &mut Value, enabled: &mut bool, options: &Value) -> Result<()> {
    if let Some(value) = options["enabled"].as_bool() {
        *enabled = value;
    }
    let properties = &options["properties"];
    for (output, input) in [
        ("supportsToolCall", &properties["supportsToolCall"]),
        (
            "supportsImages",
            &properties["inputFormat"]["supportsImage"],
        ),
        ("supportsVideo", &properties["inputFormat"]["supportsVideo"]),
    ] {
        if let Some(value) = input.as_bool() {
            model[output] = json!(value);
        }
    }
    for (output, input) in [
        ("maxInputTokens", &properties["contextWindow"]),
        (
            "maxOutputTokens",
            &options["optionSpecs"]["maxOutputTokens"]["max"],
        ),
    ] {
        if let Some(value) = input
            .as_u64()
            .filter(|value| *value > 0 && *value <= 2_000_000)
        {
            model[output] = json!(value);
        }
    }
    if let Some(levels) = options["optionSpecs"]["reasoningLevel"].get("values") {
        let levels = levels
            .as_array()
            .filter(|levels| levels.len() <= 32)
            .ok_or_else(|| Error::http(502, "ZCode 官方思考档位格式无效"))?;
        let mut names = Vec::new();
        for level in levels {
            let level = level
                .as_str()
                .filter(|value| {
                    !value.is_empty() && value.len() <= 64 && !value.chars().any(char::is_control)
                })
                .ok_or_else(|| Error::http(502, "ZCode 官方思考档位无效"))?;
            let name = match level {
                "disabled" => "none",
                "enabled" => "high",
                _ => level,
            };
            if !names.contains(&name) {
                names.push(name);
            }
        }
        model["supportsReasoning"] = json!(names.iter().any(|name| *name != "none"));
        model["reasoningLevels"] = json!(names);
    }
    Ok(())
}
