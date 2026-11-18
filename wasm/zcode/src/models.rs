//! 官方客户端发布目录经固定 CDN 读取；套餐池追加当前账号真实账单模型。
//! 静态清单仅用于尚未取得远程目录的兜底，成功空清单与读取失败分开。
use super::{account, config, network, usage_billing, usage_sources};
use agent2api_wasm_sdk::{
    self as sdk, catalog, http, json,
    state::{self, Scope},
    Error, Result, Value,
};
use regex::RegexBuilder;

const CDN: &str = "https://cdn-zcode.z.ai";
const TTL: i64 = 600_000;
const BINDING: &str = "model-catalog-binding";
fn source(record: &Value) -> String {
    config::model_verification_binding(record)
}
fn bound(record: &Value) -> Result<Value> {
    let saved = state::get(Scope::Account, BINDING, 1)?;
    Ok(
        if saved.compatible && saved.value["source"] == source(record) {
            saved.value["catalog"].clone()
        } else {
            Value::Null
        },
    )
}

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

pub fn snapshot() -> Value {
    json!({ "models": list(), "meta": { "remote": false, "refreshed_at": 0 } })
}

pub fn current(params: &Value) -> Result<Value> {
    if params["account"].as_str().is_none_or(str::is_empty) {
        return Ok(snapshot());
    }
    let record = account::record()?;
    let cached = bound(&record)?;
    account::require_current(&record)?;
    if cached["meta"]["remote"] == true && cached["models"].is_array() {
        return Ok(cached);
    }
    Ok(snapshot())
}

fn released(version: &str) -> Result<Value> {
    let response = network::request(
        "GET",
        &format!(
            "{}/api/v1/client/configs?app_version={}&platform={}",
            network::ZCODE,
            network::query(version),
            config::PLATFORM
        ),
        config::identity_headers(version, None),
        None,
    )?;
    let data = network::envelope(response, "ZCode 模型配置查询")?;
    let raw = data["configs"]["builtin_provider_config_json"]
        .as_str()
        .ok_or_else(|| Error::http(502, "ZCode 未返回官方模型配置地址"))?;
    let url = url::Url::parse(raw).map_err(|_| Error::http(502, "ZCode 模型配置地址无效"))?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.origin().ascii_serialization() != CDN
        || url.fragment().is_some()
    {
        return Err(Error::coded("ZCode 模型配置超出固定官方 CDN", "forbidden"));
    }
    let response = http::open_timeout(
        "GET",
        url.as_str(),
        json!({ "Accept": "application/json" }),
        None,
        15_000,
    )?;
    if !(200..300).contains(&response.status) {
        return Err(Error::http(502, "ZCode 官方模型配置下载失败"));
    }
    serde_json::from_slice(&response.bytes(4 * 1024 * 1024)?)
        .map_err(|_| Error::http(502, "ZCode 官方模型配置不是有效 JSON"))
}

fn items<'a>(value: &'a Value, required: bool) -> Result<&'a [Value]> {
    if value.is_null() && !required {
        return Ok(&[]);
    }
    value
        .as_array()
        .filter(|items| items.len() <= 4096)
        .map(Vec::as_slice)
        .ok_or_else(|| Error::http(502, "ZCode 官方模型规则格式或数量无效"))
}

fn id(value: &Value) -> Result<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|id| !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control))
        .ok_or_else(|| Error::http(502, "ZCode 官方模型标识无效"))
}

fn add(ids: &mut Vec<String>, name: &str) {
    if !ids.iter().any(|id| id.eq_ignore_ascii_case(name)) {
        ids.push(name.to_string());
    }
}

fn apply(model: &mut Value, source: &Value) {
    let properties = &source["properties"];
    for (field, pointer) in [
        ("supportsImages", "/inputFormat/supportsImage"),
        ("supportsVideo", "/inputFormat/supportsVideo"),
        ("supportsToolCall", "/supportsToolCall"),
    ] {
        if let Some(value) = properties.pointer(pointer).and_then(Value::as_bool) {
            model[field] = json!(value);
        }
    }
    for (field, value) in [
        ("maxInputTokens", &properties["contextWindow"]),
        (
            "maxOutputTokens",
            &source["optionSpecs"]["maxOutputTokens"]["max"],
        ),
    ] {
        if let Some(value) = value
            .as_u64()
            .filter(|value| *value > 0 && *value <= 16_000_000)
        {
            model[field] = json!(value);
        }
    }
    if let Some(levels) = source["optionSpecs"]["reasoningLevel"]["values"]
        .as_array()
        .filter(|levels| levels.len() <= 16)
    {
        let mut known = Vec::new();
        for level in levels.iter().filter_map(Value::as_str) {
            let mapped = match level {
                "disabled" => "none",
                "enabled" => "high",
                other => other,
            };
            if ["none", "minimal", "low", "medium", "high", "xhigh", "max"].contains(&mapped)
                && !known.contains(&mapped)
            {
                known.push(mapped);
            }
        }
        if !known.is_empty() {
            model["supportsReasoning"] = json!(known.iter().any(|level| *level != "none"));
            model["reasoningLevels"] = json!(known);
        }
    }
}

fn configured(document: &Value, provider: &str) -> Result<Vec<Value>> {
    let config = &document["config"];
    if !config.is_object() {
        return Err(Error::http(502, "ZCode 模型配置缺少 config"));
    }
    let rules = items(&config["providerConfigRules"]["providerRules"], true)?;
    let model_rules = &config["modelConfigRules"];
    let per_model = items(&model_rules["builtinProviderModelRules"], true)?;
    let mut ids = Vec::new();
    let mut found = false;
    for rule in rules.iter().filter(|rule| rule["providerId"] == provider) {
        found = true;
        for value in items(
            rule.get("builtinModelIds")
                .unwrap_or(&rule["config"]["builtinModelIds"]),
            false,
        )? {
            add(&mut ids, id(value)?);
        }
    }
    let mut disabled = Vec::new();
    for rule in per_model
        .iter()
        .filter(|rule| rule["providerId"] == provider)
    {
        found = true;
        let name = id(&rule["modelId"])?;
        if rule["config"]["enabled"] == false {
            add(&mut disabled, name);
        } else {
            add(&mut ids, name);
        }
    }
    if !found {
        return Err(Error::http(502, "ZCode 官方配置未包含当前套餐通道"));
    }
    ids.retain(|name| !disabled.iter().any(|id| id.eq_ignore_ascii_case(name)));
    if ids.len() > 1024 {
        return Err(Error::http(502, "ZCode 当前套餐模型数量过多"));
    }
    let mut parsed = Vec::new();
    let global_rules = items(&model_rules["modelRules"], false)?;
    if global_rules.len() > 256 {
        return Err(Error::http(502, "ZCode 官方模型匹配规则过多"));
    }
    let mut pattern_bytes = 0usize;
    for rule in global_rules {
        let pattern = rule["modelMatch"]
            .as_str()
            .filter(|pattern| pattern.len() <= 2048)
            .ok_or_else(|| Error::http(502, "ZCode 官方模型匹配规则无效"))?;
        pattern_bytes = pattern_bytes.saturating_add(pattern.len());
        if pattern_bytes > 64 * 1024 {
            return Err(Error::http(502, "ZCode 官方模型匹配规则超过总量限制"));
        }
        let matcher = RegexBuilder::new(&format!("^(?:{pattern})$"))
            .case_insensitive(true)
            .size_limit(256 * 1024)
            .dfa_size_limit(256 * 1024)
            .build()
            .map_err(|_| Error::http(502, "ZCode 官方模型匹配规则无法安全解析"))?;
        parsed.push((matcher, &rule["config"]));
    }
    Ok(ids
        .into_iter()
        .map(|name| {
            let mut model = list()
                .into_iter()
                .find(|model| {
                    model["id"]
                        .as_str()
                        .is_some_and(|id| id.eq_ignore_ascii_case(&name))
                })
                .unwrap_or_else(|| json!({ "supportsToolCall": true }));
            model["id"] = json!(name.to_ascii_lowercase());
            model["name"] = json!(name);
            for (matcher, config) in &parsed {
                if matcher.is_match(&name) {
                    apply(&mut model, config);
                }
            }
            for rule in per_model.iter().filter(|rule| {
                rule["providerId"] == provider
                    && rule["modelId"]
                        .as_str()
                        .is_some_and(|id| id.eq_ignore_ascii_case(&name))
            }) {
                apply(&mut model, &rule["config"]);
            }
            model
        })
        .collect())
}

pub fn refresh(params: &Value) -> Result<Value> {
    let record = account::record()?;
    let binding = state::get(Scope::Account, BINDING, 1)?;
    let cached = if binding.compatible && binding.value["source"] == source(&record) {
        binding.value["catalog"].clone()
    } else {
        Value::Null
    };
    let force = params["force"] == true;
    let at = cached["meta"]["refreshed_at"].as_i64().unwrap_or(0);
    if !force
        && binding.compatible
        && binding.value["source"] == source(&record)
        && cached["meta"]["remote"] == true
        && cached["models"].is_array()
        && at > 0
        && sdk::now().saturating_sub(at) < TTL
    {
        account::require_current(&record)?;
        let mut cached = cached;
        cached["refreshed"] = json!(false);
        cached["count"] = json!(cached["models"].as_array().map_or(0, Vec::len));
        return Ok(cached);
    }
    let document = released(&config::app_version()?)?;
    let provider = if account::plan(&record["secret"]) == account::START {
        "account:zai-start-plan"
    } else {
        "account:bigmodel-individual-coding-plan"
    };
    let mut models = configured(&document, provider)?;
    if account::plan(&record["secret"]) == account::START {
        let data = usage_billing::read(&record)?;
        let sources = usage_sources::billing(&data);
        let mut names = Vec::new();
        let now = sdk::now();
        for source in sources.iter().filter(|source| {
            source["endsAt"]
                .as_i64()
                .is_none_or(|at| at <= 0 || at > now)
        }) {
            for value in source["models"].as_array().into_iter().flatten() {
                add(&mut names, id(value)?);
            }
        }
        if names.is_empty() && (!data["plans"].as_array().is_none_or(Vec::is_empty)
            || !data["balances"].as_array().is_none_or(Vec::is_empty))
            && sources.iter().any(|source| source["models"].as_array().is_none_or(Vec::is_empty)
                && source["endsAt"].as_i64().is_none_or(|at| at <= 0 || at > now)) {
            return Err(Error::http(502, "ZCode 当前套餐未返回模型范围，未用空清单覆盖旧目录"));
        }
        for name in names {
            if !models.iter().any(|model| {
                model["id"]
                    .as_str()
                    .is_some_and(|id| id.eq_ignore_ascii_case(&name))
            }) {
                let mut extra = list()
                    .into_iter()
                    .find(|model| {
                        model["id"]
                            .as_str()
                            .is_some_and(|id| id.eq_ignore_ascii_case(&name))
                    })
                    .unwrap_or_else(|| json!({ "supportsToolCall": true }));
                extra["id"] = json!(name.to_ascii_lowercase());
                extra["name"] = json!(name);
                models.push(extra);
            }
        }
        models.retain(|model| {
            model["id"].as_str().is_some_and(|id| {
                sources.iter().any(|source| {
                    source["endsAt"]
                        .as_i64()
                        .is_none_or(|at| at <= 0 || at > now)
                        && source["models"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .any(|name| {
                                name.as_str()
                                    .is_some_and(|name| name.eq_ignore_ascii_case(id))
                            })
                })
            })
        });
    }
    if serde_json::to_vec(&models)
        .map_err(|_| Error::new("ZCode 目录不能序列化"))?
        .len()
        > 240 * 1024
    {
        return Err(Error::http(502, "ZCode 当前账号模型目录超过存储限制"));
    }
    let result = catalog::refreshed(models);
    account::require_current(&record)?;
    if !state::compare_exchange(
        Scope::Account,
        BINDING,
        1,
        binding.revision,
        json!({ "source": source(&record), "catalog": result }),
        0,
    )? {
        return Err(Error::http(409, "ZCode 账号目录已被其他刷新更新，请重试"));
    }
    Ok(result)
}

pub fn max_output(model: &str) -> Option<i64> {
    MODELS
        .iter()
        .find(|(id, _, _)| id.eq_ignore_ascii_case(model.trim()))
        .map(|(_, _, max)| *max)
}
