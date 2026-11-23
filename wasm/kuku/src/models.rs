//! 原生库库目录与模型名规则：正确路径model/list；不读已废弃model_list。
//! 文本思考如实回传；固定 think_mode 不代表支持客户端可调档位，工具/图片未接通。

use crate::util;
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub const FALLBACK: [&str; 13] = [
    "auto",
    "gateway-deepseek-v4.1-flash-tencent",
    "gateway-deepseek-v4-pro-tencent",
    "gateway-deepseek-v4-flash-tencent",
    "gateway-glm-5.3-flash",
    "glm-5.3",
    "gateway-glm-5.2",
    "gateway-glm-5.1-kuaishou",
    "ernie-5.1",
    "ms-kimi-k3",
    "gateway-kimi-k2.7-code-tencent",
    "gateway-kimi-k2.6",
    "ali-minimax/minimax-m3",
];
fn row(id: &str) -> Value {
    json!({ "id": id, "name": id, "supportsToolCall": false, "supportsImages": false, "supportsReasoning": false })
}
pub fn fallback() -> Value {
    json!({ "models": FALLBACK.iter().map(|id| row(id)).collect::<Vec<_>>(), "meta": { "remote": false, "refreshed_at": 0 } })
}
fn collect(value: &Value, names: &mut Vec<String>, depth: usize) -> Result<()> {
    if depth > 24 {
        return Err(Error::http(502, "库库模型目录嵌套过深"));
    }
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                let key = key.to_ascii_lowercase();
                if ["model_name", "modelname", "modelid"].contains(&key.as_str()) {
                    if let Some(name) = value
                        .as_str()
                        .map(str::trim)
                        .filter(|name| util::identifier(name, 256))
                    {
                        names.push(name.into());
                    }
                } else {
                    collect(value, names, depth + 1)?;
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                collect(value, names, depth + 1)?;
            }
        }
        _ => {}
    }
    if names.len() > 2048 {
        return Err(Error::http(502, "库库模型数量超过上限"));
    }
    Ok(())
}
pub fn parse(value: &Value, at: i64) -> Result<Value> {
    let mut names = Vec::new();
    collect(&value["data"], &mut names, 0)?;
    names.sort();
    names.dedup();
    if names.is_empty()
        && !value["data"].as_array().is_some_and(Vec::is_empty)
        && !value["data"]["model_list"]
            .as_array()
            .is_some_and(Vec::is_empty)
    {
        return Err(Error::http(502, "库库远程目录没有可识别模型，保留上次清单"));
    }
    Ok(
        json!({ "models": names.iter().map(|name| row(name)).collect::<Vec<_>>(), "meta": { "remote": true, "refreshed_at": at } }),
    )
}
pub fn valid(value: &Value) -> bool {
    value["models"].as_array().is_some_and(|rows| {
        rows.len() <= 2048
            && rows.iter().all(|row| {
                row["id"]
                    .as_str()
                    .is_some_and(|id| util::identifier(id, 256))
            })
    })
}
pub fn resolve(list: &Value, requested: &str) -> Result<(String, Option<String>)> {
    let requested = requested
        .trim()
        .strip_prefix("kuku/")
        .unwrap_or(requested.trim())
        .trim();
    if !requested.is_empty() && !util::identifier(requested, 256) {
        return Err(Error::http(400, "库库模型名称无效"));
    }
    if list["meta"]["remote"] == true && list["models"].as_array().is_some_and(Vec::is_empty) {
        return Err(Error::http(
            403,
            "当前库库账号的在线目录为空，未用其它账号或静态型号代替",
        ));
    }
    if requested.is_empty() {
        return Ok(("auto".into(), None));
    }
    if (list["meta"]["remote"] != true && FALLBACK.contains(&requested))
        || list["models"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|row| row["id"] == requested)
    {
        Ok((requested.into(), None))
    } else {
        // 保持原生回落语义，但明确告诉客户端实际使用auto，不伪称仍用了请求型号。
        Ok((
            "auto".into(),
            Some("库库目录没有请求的型号，按原生规则使用auto".into()),
        ))
    }
}
