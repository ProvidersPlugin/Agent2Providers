//! 国际账号目录仅展示配置，不推断国内套餐前缀或完整调用权限。
//! 显示 ID 与真实 UUID 分开；白名单没有 api_key/base_url/owner 或上游原始对象。

use crate::{
    endpoints::{self, ReadEndpoint},
    errors,
    redact::Guard,
    util,
};
use agent2api_wasm_sdk::{json, Error, Result, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_ROWS: usize = 4096;
pub const MAX_CACHE: usize = 240 * 1024;
const ROW_FIELDS: &[&str] = &[
    "id",
    "name",
    "provider",
    "model",
    "upstreamId",
    "interfaceType",
    "isDefault",
    "isFree",
    "accessLevel",
    "upstreamEnabled",
    "upstreamImageSupport",
    "upstreamThinkingEnabled",
    "enable",
    "eligibilityKnown",
    "eligibilityReason",
    "supportsToolCall",
    "supportsImages",
    "supportsVideo",
    "supportsReasoning",
    "chatReady",
    "catalogScope",
    "catalogProvider",
    "maxInputTokens",
    "maxOutputTokens",
    "ambiguous",
];

fn boolean(row: &Value, key: &str) -> Result<Option<bool>> {
    match row.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        _ => Err(errors::protocol(
            ReadEndpoint::Models,
            "模型布尔字段格式变化",
        )),
    }
}

fn string<'a>(row: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match row.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if !text.chars().any(char::is_control) => Ok(Some(text.trim())),
        _ => Err(errors::protocol(
            ReadEndpoint::Models,
            "模型文本字段格式变化",
        )),
    }
}

pub fn project(row: &Value, guard: &Guard) -> Result<Option<Value>> {
    if !row.is_object() {
        return Err(errors::protocol(ReadEndpoint::Models, "模型条目不是对象"));
    }
    if boolean(row, "is_hidden")? == Some(true) {
        return Ok(None);
    }
    let name = string(row, "model")?
        .filter(|name| !name.is_empty())
        .ok_or_else(|| errors::protocol(ReadEndpoint::Models, "缺少模型名"))?;
    let provider = string(row, "provider")?.unwrap_or("");
    let upstream = string(row, "id")?
        .filter(|id| util::uuid(id))
        .ok_or_else(|| errors::protocol(ReadEndpoint::Models, "缺少有效模型 UUID"))?;
    if name.len() > 384
        || provider.len() > 96
        || [name, provider, upstream]
            .iter()
            .any(|value| guard.contains(value))
    {
        return Err(errors::protocol(
            ReadEndpoint::Models,
            "模型标识超限或含受保护内容",
        ));
    }
    let enabled = boolean(row, "enabled")?;
    let enable = boolean(row, "enable")?;
    if enabled
        .zip(enable)
        .is_some_and(|(left, right)| left != right)
    {
        return Err(errors::protocol(
            ReadEndpoint::Models,
            "模型启用状态互相矛盾",
        ));
    }
    let upstream_enabled = enabled.or(enable);
    let id = if provider.is_empty() {
        name.to_string()
    } else {
        format!("{provider}/{name}")
    };
    let label = string(row, "display_name")?
        .filter(|text| !text.is_empty())
        .or(string(row, "remark")?.filter(|text| !text.is_empty()))
        .unwrap_or(name);
    let mut projected = json!({
        "id": id, "name": guard.text(label, 256), "provider": provider, "model": name,
        "upstreamId": upstream.to_ascii_lowercase(),
        "interfaceType": guard.text(string(row, "interface_type")?.unwrap_or(""), 64),
        "isDefault": boolean(row, "is_default")?, "isFree": boolean(row, "is_free")?,
        "accessLevel": guard.text(string(row, "access_level")?.unwrap_or(""), 64),
        "upstreamEnabled": upstream_enabled,
        "upstreamImageSupport": boolean(row, "support_image")?,
        "upstreamThinkingEnabled": boolean(row, "thinking_enabled")?,
        "enable": if upstream_enabled == Some(false) { Some(false) } else { None },
        "eligibilityKnown": false,
        "eligibilityReason": if upstream_enabled == Some(false) { "upstream_disabled" } else { "international_permissions_unverified" },
        "supportsToolCall": true, "supportsImages": false, "supportsVideo": false,
        "supportsReasoning": boolean(row, "thinking_enabled")?.unwrap_or(false), "chatReady": true, "catalogScope": "display_only", "ambiguous": false,
        "catalogProvider": endpoints::ID
    });
    for (source, target) in [
        ("context_limit", "maxInputTokens"),
        ("output_limit", "maxOutputTokens"),
    ] {
        if let Some(number) = util::integer(row, source).filter(|number| *number > 0) {
            projected[target] = json!(number);
        }
    }
    Ok(Some(projected))
}

pub fn finish(rows: Vec<Value>, at: i64) -> Result<Value> {
    let mut groups: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut upstream_seen: BTreeMap<String, String> = BTreeMap::new();
    for row in rows {
        let uuid = row["upstreamId"].as_str().unwrap_or("").to_string();
        let encoded = row.to_string();
        if upstream_seen
            .get(&uuid)
            .is_some_and(|previous| previous != &encoded)
        {
            return Err(errors::protocol(
                ReadEndpoint::Models,
                "同一UUID在分页间的名称或属性改变",
            ));
        }
        upstream_seen.insert(uuid, encoded);
        let group = groups
            .entry(row["id"].as_str().unwrap_or("").to_ascii_lowercase())
            .or_default();
        if let Some(previous) = group
            .iter()
            .find(|previous| previous["upstreamId"] == row["upstreamId"])
        {
            if previous != &row {
                return Err(errors::protocol(
                    ReadEndpoint::Models,
                    "同一模型在分页间改变",
                ));
            }
        } else {
            group.push(row);
        }
    }
    let mut visible = Vec::new();
    let mut models = Vec::new();
    let mut conflicts = 0;
    let mut disabled = 0;
    for group in groups.into_values() {
        let ambiguous = group.len() > 1;
        if ambiguous {
            conflicts += 1;
        }
        for mut row in group {
            if ambiguous {
                row["ambiguous"] = json!(true);
                row["enable"] = json!(false);
                row["eligibilityReason"] = json!("ambiguous_display_id");
            } else if row["upstreamEnabled"] != false {
                models.push(row.clone());
            } else {
                disabled += 1;
            }
            visible.push(row);
        }
    }
    let list = json!({ "schema": 1, "provider": endpoints::ID, "models": models, "visibleModels": visible,
        "scope": "display_only", "completeAccountPermissions": false, "paginationComplete": true,
        "ambiguousIds": conflicts, "disabledModels": disabled, "eligibilityKnown": false,
        "meta": { "remote": true, "refreshed_at": at }, "stale": false,
        "warnings": ["模型目录按账号和国际站隔离；目录展示不代表完整权限，实际任务由原账号和明确授权执行。"] });
    if list.to_string().len() > MAX_CACHE {
        return Err(too_large());
    }
    Ok(list)
}

pub fn empty() -> Value {
    json!({ "schema": 1, "provider": endpoints::ID, "models": [], "visibleModels": [],
        "scope": "display_only", "completeAccountPermissions": false, "paginationComplete": null,
        "meta": { "remote": false, "refreshed_at": 0 }, "stale": true })
}

const LIST_FIELDS: &[&str] = &[
    "schema",
    "provider",
    "models",
    "visibleModels",
    "scope",
    "completeAccountPermissions",
    "paginationComplete",
    "ambiguousIds",
    "disabledModels",
    "eligibilityKnown",
    "meta",
    "stale",
    "warnings",
];

fn valid_row(row: &Value, guard: Option<&Guard>) -> bool {
    let Some(fields) = row.as_object() else {
        return false;
    };
    let Some(id) = row["id"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 481)
    else {
        return false;
    };
    if !fields.keys().all(|key| ROW_FIELDS.contains(&key.as_str())) {
        return false;
    }
    for (key, value) in fields {
        let typed = match key.as_str() {
            "isDefault"
            | "isFree"
            | "upstreamEnabled"
            | "upstreamImageSupport"
            | "upstreamThinkingEnabled"
            | "enable" => value.is_null() || value.is_boolean(),
            "eligibilityKnown" | "supportsToolCall" | "supportsImages" | "supportsVideo"
            | "supportsReasoning" | "chatReady" | "ambiguous" => value.is_boolean(),
            "maxInputTokens" | "maxOutputTokens" => value
                .as_i64()
                .is_some_and(|limit| limit > 0 && limit <= util::MAX_SAFE_INTEGER),
            _ => value
                .as_str()
                .is_some_and(|text| text.len() <= 1024 && !text.chars().any(char::is_control)),
        };
        if !typed {
            return false;
        }
    }
    let Some(provider) = row["provider"]
        .as_str()
        .filter(|provider| provider.len() <= 96)
    else {
        return false;
    };
    let Some(model) = row["model"]
        .as_str()
        .filter(|model| !model.is_empty() && model.len() <= 384)
    else {
        return false;
    };
    let expected = if provider.is_empty() {
        model.to_string()
    } else {
        format!("{provider}/{model}")
    };
    id == expected
        && row["upstreamId"].as_str().is_some_and(util::uuid)
        && row["catalogScope"] == "display_only"
        && row["catalogProvider"] == endpoints::ID
        && row["eligibilityKnown"] == false
        && row["enable"] != true
        && row["chatReady"].is_boolean()
        && row["supportsToolCall"].is_boolean()
        && row["supportsReasoning"].is_boolean()
        && row["supportsImages"] == false
        && row["supportsVideo"] == false
        && guard.is_none_or(|guard| {
            !fields
                .values()
                .any(|value| value.as_str().is_some_and(|text| guard.contains(text)))
        })
}

fn unique_rows(rows: &[Value], guard: Option<&Guard>) -> bool {
    let mut ids = BTreeSet::new();
    rows.len() <= MAX_ROWS
        && rows.iter().all(|row| {
            valid_row(row, guard)
                && row["enable"] != false
                && row["ambiguous"] == false
                && ids.insert(row["id"].as_str().unwrap_or("").to_ascii_lowercase())
        })
}

pub fn valid(list: &Value, guard: Option<&Guard>) -> bool {
    let metadata = list["meta"].as_object().is_some_and(|meta| {
        meta.keys()
            .all(|key| ["remote", "refreshed_at"].contains(&key.as_str()))
    });
    let warnings = list.get("warnings").is_none_or(|value| {
        value.as_array().is_some_and(|rows| {
            rows.len() <= 16
                && rows.iter().all(|row| {
                    row.as_str().is_some_and(|text| {
                        text.len() <= 2048 && guard.is_none_or(|guard| !guard.contains(text))
                    })
                })
        })
    });
    list.as_object()
        .is_some_and(|fields| fields.keys().all(|key| LIST_FIELDS.contains(&key.as_str())))
        && metadata
        && warnings
        && list["provider"] == endpoints::ID
        && list["schema"] == 1
        && list["scope"] == "display_only"
        && list["completeAccountPermissions"] == false
        && list["meta"]["remote"].is_boolean()
        && list["meta"]["refreshed_at"]
            .as_i64()
            .is_some_and(|at| at >= 0 && at <= util::MAX_SAFE_INTEGER)
        && (list["meta"]["remote"] != true || list["paginationComplete"] == true)
        && list["models"]
            .as_array()
            .is_some_and(|rows| unique_rows(rows, guard))
        && list["visibleModels"].as_array().is_some_and(|rows| {
            rows.len() <= MAX_ROWS && rows.iter().all(|row| valid_row(row, guard))
        })
        && list.to_string().len() <= MAX_CACHE
}

pub fn next(data: &Value, seen: &mut BTreeSet<String>, guard: &Guard) -> Result<Option<String>> {
    let page = data
        .get("page")
        .filter(|page| page.is_object())
        .ok_or_else(|| errors::protocol(ReadEndpoint::Models, "缺少明确分页对象，不能宣称完整"))?;
    if let Some(raw) = page.get("cursor").filter(|value| !value.is_null()) {
        if !raw.as_str().is_some_and(|cursor| {
            cursor.len() <= 4096 && !cursor.chars().any(char::is_control) && !guard.contains(cursor)
        }) {
            return Err(errors::protocol(ReadEndpoint::Models, "分页游标结构无效"));
        }
    }
    match page["has_next_page"].as_bool() {
        Some(false) => Ok(None),
        Some(true) => {
            let cursor = page["cursor"]
                .as_str()
                .filter(|cursor| {
                    !cursor.is_empty()
                        && cursor.len() <= 4096
                        && !cursor.chars().any(char::is_control)
                        && !guard.contains(cursor)
                })
                .ok_or_else(|| errors::protocol(ReadEndpoint::Models, "下一页游标无效"))?;
            if !seen.insert(cursor.to_string()) {
                return Err(errors::protocol(ReadEndpoint::Models, "下一页游标重复"));
            }
            Ok(Some(cursor.to_string()))
        }
        None => Err(errors::protocol(
            ReadEndpoint::Models,
            "缺少布尔分页完成标志",
        )),
    }
}

pub fn too_large() -> Error {
    errors::protocol(
        ReadEndpoint::Models,
        "目录超出页数、条数、字节或投影上限，未发布截断清单",
    )
}
