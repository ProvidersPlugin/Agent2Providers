//! 仅服务 Cline 免费池，读取/写回主机绑定的 provider 目录。
//! 响应中的 clinePass 分组与 cline-pass/ / cline-cloud/ 通道模型一并排除，避免混用；未知图片和输出上限不伪造能力。

use super::client;
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use std::collections::HashSet;

fn check_provider() -> Result<()> {
    if crate::PROVIDER_ID == "cline-free" {
        Ok(())
    } else {
        Err(Error::http(500, "Cline 免费池身份未登记"))
    }
}

const FREE_FALLBACK: &[(&str, &str, i64, bool)] = &[
    (
        "cline-free/deepseek-v4.1-flash",
        "DeepSeek V4.1 Flash (免费)",
        1_000_000,
        true,
    ),
    (
        "cline-free/muse-spark-1.3-contributor",
        "Muse Spark 1.3 (免费)",
        200_000,
        true,
    ),
    (
        "cline-free/solar-pro4",
        "Solar Pro 4 (免费)",
        128_000,
        false,
    ),
    (
        "z-ai/glm-5.3-flash",
        "GLM-5.3-Flash (免费)",
        1_310_720,
        true,
    ),
    (
        "poolside/laguna-s-2.1:free",
        "Laguna S 2.1 (免费)",
        262_144,
        true,
    ),
];

fn entry(id: &str, name: &str, context: i64, reasoning: bool) -> Value {
    json!({ "id": id, "name": name, "maxInputTokens": context,
        "supportsToolCall": true, "supportsReasoning": reasoning, "isDefault": false, "kind": "chat" })
}

pub fn defaults() -> Vec<Value> {
    if check_provider().is_err() {
        return Vec::new();
    }
    FREE_FALLBACK
        .iter()
        .map(|(id, name, context, reasoning)| entry(id, name, *context, *reasoning))
        .collect()
}

fn context(id: &str) -> i64 {
    if let Some((_, _, context, _)) = FREE_FALLBACK
        .iter()
        .find(|(key, ..)| key.eq_ignore_ascii_case(id))
    {
        return *context;
    }
    let id = id.to_ascii_lowercase();
    // 原生目录的既有家族兜底口径，不增加未经验证的视觉/输出长度承诺。
    if id.contains("deepseek-v4.1") || id.contains("1m") {
        1_000_000
    } else if id.contains("kimi") || id.contains("qwen3.8") || id.contains("glm-5") {
        256_000
    } else if id.contains("claude") || id.contains("gpt-6") || id.contains("grok") {
        200_000
    } else if id.contains("minimax") || id.contains("mimo") {
        1_000_000
    } else {
        128_000
    }
}

fn inferred_free(id: &str) -> bool {
    id.starts_with("cline-free/")
        || FREE_FALLBACK
            .iter()
            .any(|(known, ..)| known.eq_ignore_ascii_case(id))
}

fn excluded(id: &str) -> bool {
    id.starts_with("cline-cloud/") || id.starts_with("cline-pass/")
}

pub fn cached() -> Result<Value> {
    check_provider()?;
    let mut value = sdk::catalog::cached()?;
    if let Some(models) = value["models"].as_array_mut() {
        models.retain(|row| row["id"].as_str().is_some_and(|id| !excluded(id)));
        if value["meta"].is_object() {
            return Ok(value);
        }
    }
    Ok(json!({ "models": defaults(), "meta": { "remote": false, "refreshed_at": 0 } }))
}

struct CatalogRow<'a> {
    value: &'a Value,
    id: &'a str,
}

fn parse_group<'a>(payload: &'a Value, key: &str, required: bool) -> Result<Vec<CatalogRow<'a>>> {
    let Some(value) = payload.get(key) else {
        return if required {
            Err(Error::http(502, format!("Cline 目录缺少 {key} 分组")))
        } else {
            Ok(Vec::new())
        };
    };
    let rows = value
        .as_array()
        .ok_or_else(|| Error::http(502, format!("Cline 目录 {key} 分组必须是数组")))?;
    if rows.len() > 4096 {
        return Err(Error::http(502, format!("Cline 目录 {key} 分组条目过多")));
    }
    rows.iter()
        .map(|row| {
            let raw = row
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::http(502, "Cline 模型条目缺少文本标识"))?;
            let id = raw.trim();
            // 先校验原文，不能靠trim掩盖换行、制表符或超长标识。
            if id.is_empty() || raw.len() > 512 || raw.chars().any(char::is_control) {
                return Err(Error::http(502, "Cline 模型标识格式异常"));
            }
            Ok(CatalogRow { value: row, id })
        })
        .collect()
}

fn parse(payload: &Value) -> Result<Vec<Value>> {
    // 只读同次公开响应。缺少可选分组仍兼容；显式null/坏数组不能当作空组。
    // 归属和去重依赖这些id，必须整组校验完再筛选，不能发布残缺的成功快照。
    let group = parse_group(payload, "free", true)?;
    let other = parse_group(payload, "clinePass", false)?;
    let recommended = parse_group(payload, "recommended", false)?;
    let other_ids: HashSet<&str> = other.iter().map(|row| row.id).collect();
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for (rows, grouped) in [(&group, true), (&recommended, false)] {
        for row in rows {
            let id = row.id;
            let belongs_elsewhere = other_ids.contains(id) || !grouped && !inferred_free(id);
            if excluded(id) || belongs_elsewhere {
                continue;
            }
            if !seen.insert(id.to_string()) {
                continue;
            }
            // 展示名等可选元数据继续容错；严格拒绝只针对身份与归属证据。
            let name = row.value["name"]
                .as_str()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(id);
            let lower = id.to_ascii_lowercase();
            models.push(entry(
                id,
                name,
                context(id),
                !(lower.contains("solar-pro4") || lower.contains("flash-lite")),
            ));
        }
    }
    // 结构有效且筛选后确实为空时保留空快照，不用静态表伪造远程模型。
    Ok(models)
}

pub fn refresh() -> Result<Value> {
    check_provider()?;
    let result = client::request("GET", "/ai/cline/recommended-models", None, true, None).and_then(
        |reply| {
            if reply.success() {
                parse(&reply.data()?)
            } else {
                Err(Error::http(502, "Cline 免费池目录请求失败"))
            }
        },
    );
    match result {
        Ok(models) => Ok(
            json!({ "refreshed": true, "count": models.len(), "models": models,
            "meta": { "remote": true, "refreshed_at": sdk::now() } }),
        ),
        Err(error) => {
            let mut previous = cached()?;
            previous["refreshed"] = json!(false);
            previous["count"] = json!(previous["models"].as_array().map(Vec::len).unwrap_or(0));
            previous["message"] = json!(error.message);
            Ok(previous)
        }
    }
}

pub fn wire_id(requested: &str) -> Result<String> {
    check_provider()?;
    if excluded(requested) {
        return Err(Error::http(400, "免费池不会转发订阅池或云池通道").with_code("local_precondition"));
    }
    let list = cached()?;
    let row = list["models"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| row["id"] == requested || row["name"] == requested)
        .ok_or_else(|| {
            Error::http(400, "Cline 免费池目录没有该模型，请刷新目录或检查映射")
                .with_code("local_precondition")
        })?;
    row["id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| Error::http(502, "Cline 免费池目录格式异常"))
}
