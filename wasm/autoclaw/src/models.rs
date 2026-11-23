//! 真实目录在 /autoclaw-proxy/proxy/autoclaw-model-config，顶层 models，无 code/data 信封。
//! 远程快照由宿主 catalog.v1 按 provider 持久化；这不是账号权限清单，不声明 catalog.accounts.v1。
//! 失败保留成功快照，五分钟内不重复拉；最低两项静态清单来自当前原生国内实现。
use super::{account, client, config, endpoints, util};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub const DEFAULT_ROUTE: &str = "zai_auto";
const PREFIXES: [&str; 2] = ["zaicoding_", "zai_"];
const TTL: i64 = 300_000;

pub fn defaults() -> Vec<Value> {
    vec![
        json!({ "id": "glm-5.3", "name": "GLM-5.3", "routeId": "zaicoding_glm-5.3",
            "maxInputTokens": 1_048_576, "maxOutputTokens": 307_200, "supportsImages": false,
            "supportsReasoning": true, "supportsToolCall": true }),
        json!({ "id": "glm-5.3-flash", "name": "GLM-5.3-Flash", "routeId": "zai_glm-5.3-flash",
            "maxInputTokens": 1_048_576, "maxOutputTokens": 131_072, "supportsImages": true,
            "supportsReasoning": true, "supportsToolCall": true }),
    ]
}
pub fn strip(route: &str) -> &str {
    for prefix in PREFIXES {
        if let Some(name) = route.strip_prefix(prefix) {
            return name;
        }
    }
    route
}
pub fn route_shape(value: &str) -> bool {
    let Some((prefix, tail)) = value.split_once('_') else {
        return false;
    };
    !prefix.is_empty()
        && prefix
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_lowercase)
        && prefix
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && !tail.is_empty()
        && tail.len() <= 127
        && tail
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
}
fn valid_model(value: &str) -> bool {
    !value.trim().is_empty() && util::printable(value, 512)
}

pub fn cached() -> Result<Value> {
    let value = sdk::catalog::cached()?;
    if value["meta"]["remote"] == true && value["models"].is_array() {
        // 已确认的空目录不是失败，不复活静态模型。
        return Ok(value);
    }
    Ok(json!({ "models": defaults(), "meta": { "remote": false, "refreshed_at": 0 } }))
}
fn entry(value: &Value) -> Option<Value> {
    let route = value
        .as_str()
        .unwrap_or_else(|| value["id"].as_str().unwrap_or(""))
        .trim();
    if !valid_model(route) || strip(route).is_empty() {
        return None;
    }
    let id = strip(route);
    let name = util::text(value, &["name"]);
    if !name.is_empty() && (name.len() > 512 || name.chars().any(char::is_control)) {
        return None;
    }
    let mut result = json!({ "id": id, "name": if name.is_empty() { id.to_uppercase() } else { name },
        "routeId": route, "supportsToolCall": true, "kind": "chat" });
    if value.is_object() {
        result["supportsImages"] = json!(value["input"]
            .as_array()
            .is_some_and(|items| items.iter().any(|value| value == "image")));
        result["supportsReasoning"] = json!(value["reasoning"].as_bool().unwrap_or(true));
        for (input, output) in [
            ("contextWindow", "maxInputTokens"),
            ("maxTokens", "maxOutputTokens"),
        ] {
            if let Some(number) = value[input].as_i64().filter(|number| *number > 0) {
                result[output] = json!(number);
            }
        }
        let level = util::text(value, &["creditConsumptionLevel"]);
        if !level.is_empty() {
            result["credits"] = json!(level);
        } // 低/中/高是文案，不虚构数值倍率。
    }
    Some(result)
}
pub fn refresh(params: &Value) -> Result<Value> {
    let previous = cached()?;
    let force = params["force"].as_bool().unwrap_or(false);
    let age = sdk::now().saturating_sub(previous["meta"]["refreshed_at"].as_i64().unwrap_or(0));
    if !force && previous["meta"]["remote"] == true && (0..TTL).contains(&age) {
        return Ok(sdk::catalog::unchanged(
            previous["models"].as_array().map(Vec::len).unwrap_or(0),
        ));
    }
    if params["account"].as_str().is_none_or(str::is_empty) {
        return Ok(
            json!({ "refreshed": false, "count": previous["models"].as_array().map(Vec::len).unwrap_or(0),
            "message": "没有主程序选定的国内账号，已保留模型清单" }),
        );
    }
    let result = (|| -> Result<Vec<Value>> {
        let snapshot = account::read()?;
        let payload = client::get(
            endpoints::MODELS,
            &account::access(&snapshot.secret),
            "模型目录查询",
        )?;
        let entries = payload["models"]
            .as_array()
            .filter(|items| items.len() <= 2048)
            .ok_or_else(|| Error::http(502, "AutoClaw 目录缺少有效的 models 数组"))?;
        let models = entries
            .iter()
            .map(entry)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                Error::http(502, "AutoClaw 目录含有无法识别的模型条目，已保留当前清单")
            })?;
        let current = account::read()?;
        if !account::same(&snapshot, &current) {
            return Err(Error::http(409, "AutoClaw 目录查询期间账号已变化"));
        }
        Ok(models)
    })();
    match result {
        Ok(models) => Ok(sdk::catalog::refreshed(models)),
        Err(error) if error.code.as_deref() == Some("forbidden") => Err(error),
        Err(error) => Ok(
            json!({ "refreshed": false, "count": previous["models"].as_array().map(Vec::len).unwrap_or(0), "message": error.message }),
        ),
    }
}

pub struct Route {
    pub route: String,
    pub body: String,
    pub requested: String,
}
fn matched(models: &[Value], name: &str) -> Option<String> {
    for field in ["id", "name", "routeId"] {
        if let Some(model) = models.iter().find(|item| {
            item[field]
                .as_str()
                .is_some_and(|value| value.trim().eq_ignore_ascii_case(name))
        }) {
            let route = util::text(model, &["routeId", "id"]);
            if valid_model(&route) {
                return Some(route);
            }
        }
    }
    None
}
pub fn route(raw: &str) -> Result<Route> {
    let requested = raw.trim();
    let fallback = config::default_route()?;
    if requested.is_empty() {
        return Ok(Route {
            body: strip(&fallback).into(),
            requested: fallback.clone(),
            route: fallback,
        });
    }
    if !valid_model(requested) {
        return Err(Error::http(400, "AutoClaw 模型名称格式无效"));
    }
    let remote = cached()?;
    // 在线目录给出的当前路由优先，静态表只作未取得远程目录时的兜底。
    let route = remote["models"]
        .as_array()
        .and_then(|models| matched(models, requested))
        .or_else(|| {
            (PREFIXES.iter().any(|prefix| requested.starts_with(*prefix)) || route_shape(requested))
                .then(|| requested.to_string())
        })
        .unwrap_or(fallback);
    // 与原生插件一致：没有主程序自定义模型表，不假造 is_custom 判定；显式 routeId 原样使用。
    Ok(Route {
        body: strip(&route).into(),
        requested: requested.into(),
        route,
    })
}
