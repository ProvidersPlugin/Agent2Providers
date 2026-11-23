//! 展示清单不是完整账号权限目录；catalog.accounts.v1 只用于同账号保存与聚合。
//! 官方模型可能带 api_key/base_url，必须字段白名单投影。支持源码中的 is_hidden、
//! 订阅内置档位判断，兼容显式 enable/enabled=false；未知权益不广告为可用。
//! 来源：chaitin/MonkeyCode ad169c9... frontend/src/utils/common.tsx 与 Api.ts；
//! {provider}/{model} -> UUID 沿本仓原生和 Apache 历史对照，绝不模糊回落另一模型。

use std::collections::{BTreeMap, BTreeSet};

use agent2api_wasm_sdk::{json, Error, Result, Value};

use crate::{endpoints::ReadEndpoint, errors, util};

pub const MAX_ROWS: usize = 4096;

fn uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
        && value
            .bytes()
            .any(|byte| matches!(byte, b'1'..=b'9' | b'a'..=b'f' | b'A'..=b'F'))
}

fn tier(value: &str) -> Option<u8> {
    match value {
        "basic" => Some(0),
        "pro" => Some(1),
        "ultra" | "flagship" => Some(2),
        _ => None,
    }
}

/// 只反映本次目录可见性与已知订阅门槛；不是上游任务授权保证。
fn eligibility(row: &Value, subscription: Option<&Value>) -> (Option<bool>, &'static str) {
    for key in ["enable", "enabled"] {
        match row.get(key) {
            Some(Value::Bool(false)) => return (Some(false), "upstream_disabled"),
            Some(Value::Bool(true)) | None | Some(Value::Null) => {}
            _ => return (None, "invalid_enable_field"),
        }
    }
    // 官方 canUseModelBySubscription 只按内置套餐前缀判断；普通配置的
    // access_level 是展示字段，不扩大为额外资格门而隐藏用户模型。
    let model = util::text(row, &["model"]).to_ascii_lowercase();
    let required: u8 = if model.starts_with("monkeycode-ultra") {
        2
    } else if model.starts_with("monkeycode-pro") {
        1
    } else {
        0
    };
    if required == 0 {
        return (Some(true), "visible_without_known_subscription_gate");
    }
    let Some(subscription) = subscription else {
        return (None, "subscription_unavailable");
    };
    let plan = util::text(subscription, &["plan"]);
    let Some(actual) = tier(plan) else {
        return (None, "subscription_plan_unknown");
    };
    if !subscription["expires_at"].is_null() && subscription["expiresAt"].is_null() {
        return (None, "subscription_expiry_unknown");
    }
    if let Some(expiry) = subscription["expiresAt"].as_i64() {
        let Some(observed) = subscription["observedAt"].as_i64() else {
            return (None, "subscription_time_unknown");
        };
        if expiry <= observed {
            return (None, "subscription_expiry_requires_confirmation");
        }
    }
    (
        Some(actual >= required),
        if actual >= required {
            "subscription_matches"
        } else {
            "subscription_required"
        },
    )
}

fn bool_field(row: &Value, key: &str) -> Result<Option<bool>> {
    match row.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        _ => Err(errors::protocol(
            ReadEndpoint::Models,
            "模型布尔字段格式无效",
        )),
    }
}

pub fn project(row: &Value, subscription: Option<&Value>, session: &str) -> Result<Option<Value>> {
    if !row.is_object() {
        return Err(errors::protocol(ReadEndpoint::Models, "模型条目不是对象"));
    }
    if bool_field(row, "is_hidden")? == Some(true) {
        return Ok(None);
    }
    let model = util::text(row, &["model"]);
    let provider = util::text(row, &["provider"]);
    let upstream = util::text(row, &["id"]);
    if model.is_empty()
        || model.len() > 384
        || provider.len() > 96
        || !uuid(upstream)
        || [model, provider, upstream].iter().any(|value| {
            value.chars().any(char::is_control) || !session.is_empty() && value.contains(session)
        })
    {
        return Err(errors::protocol(
            ReadEndpoint::Models,
            "模型名称或上游 UUID 缺失／无效，保留旧清单",
        ));
    }
    let id = if provider.is_empty() {
        model.to_string()
    } else {
        format!("{provider}/{model}")
    };
    let label = util::text(row, &["display_name", "remark"]);
    let (enabled, reason) = eligibility(row, subscription);
    let mut entry = json!({
        "id": id,
        "name": util::public_text(if label.is_empty() { model } else { label }, session, 256),
        "provider": provider, "model": model, "upstreamId": upstream,
        "interfaceType": util::public_text(util::text(row, &["interface_type"]), session, 64),
        "cliName": "opencode",
        "isDefault": bool_field(row, "is_default")?,
        "isFree": bool_field(row, "is_free")?,
        "accessLevel": util::public_text(util::text(row, &["access_level"]), session, 64),
        "enable": enabled, "eligibilityKnown": enabled.is_some(), "eligibilityReason": reason,
        "upstreamImageSupport": bool_field(row, "support_image")?,
        "upstreamThinkingEnabled": bool_field(row, "thinking_enabled")?,
        "supportsToolCall": true, "supportsImages": false, "supportsVideo": false,
        "supportsReasoning": bool_field(row, "thinking_enabled")?.unwrap_or(false), "chatReady": true,
        "capabilityState": "complete", "catalogScope": "display_only"
    });
    if let Some(limit) = util::positive_limit(row, "context_limit") {
        entry["maxInputTokens"] = json!(limit);
    }
    if let Some(limit) = util::positive_limit(row, "output_limit") {
        entry["maxOutputTokens"] = json!(limit);
    }
    Ok(Some(entry))
}

/// 重复显示 ID 但 UUID 不同不能任选一个；该 ID 从广告剔除并在账号审阅快照保留。
pub fn finish(rows: Vec<Value>, at: i64, pagination_complete: Option<bool>) -> Result<Value> {
    let mut groups: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for row in rows {
        let key = util::text(&row, &["id"]).to_ascii_lowercase();
        let group = groups.entry(key).or_default();
        if let Some(previous) = group
            .iter()
            .find(|previous| previous["upstreamId"] == row["upstreamId"])
        {
            if previous != &row {
                return Err(errors::protocol(
                    ReadEndpoint::Models,
                    "同一模型在分页期间发生变化，请重新刷新",
                ));
            }
        } else {
            group.push(row);
        }
    }
    let mut visible = Vec::new();
    let mut advertised = Vec::new();
    let mut ambiguous = 0;
    for group in groups.into_values() {
        let conflict = group.len() > 1;
        if conflict {
            ambiguous += 1;
        }
        for mut row in group {
            if conflict {
                row["enable"] = json!(false);
                row["eligibilityReason"] = json!("ambiguous_display_id");
            } else if row["enable"] == true {
                advertised.push(row.clone());
            }
            visible.push(row);
        }
    }
    let unavailable = visible.iter().filter(|row| row["enable"] != true).count();
    Ok(
        json!({ "schema": 1, "provider": "monkeycode", "models": advertised,
        "visibleModels": visible, "scope": "display_only", "completeAccountPermissions": false,
        "paginationComplete": pagination_complete, "filteredOrUnknown": unavailable,
        "ambiguousIds": ambiguous, "meta": { "remote": true, "refreshed_at": at } }),
    )
}

pub fn empty() -> Value {
    json!({ "models": [], "meta": { "remote": false, "refreshed_at": 0 },
        "scope": "display_only", "completeAccountPermissions": false })
}

pub fn valid(list: &Value) -> bool {
    const FIELDS: &[&str] = &[
        "id",
        "name",
        "provider",
        "model",
        "upstreamId",
        "interfaceType",
        "cliName",
        "isDefault",
        "isFree",
        "accessLevel",
        "enable",
        "eligibilityKnown",
        "eligibilityReason",
        "upstreamImageSupport",
        "upstreamThinkingEnabled",
        "supportsToolCall",
        "supportsImages",
        "supportsVideo",
        "supportsReasoning",
        "chatReady",
        "capabilityState",
        "catalogScope",
        "maxInputTokens",
        "maxOutputTokens",
    ];
    list["meta"]["remote"].is_boolean()
        && list["meta"]["refreshed_at"]
            .as_i64()
            .is_some_and(|at| at >= 0)
        && list["models"].as_array().is_some_and(|rows| {
            rows.len() <= MAX_ROWS
                && rows.iter().all(|row| {
                    row.as_object().is_some_and(|fields| {
                        fields.keys().all(|key| FIELDS.contains(&key.as_str()))
                    }) && !util::text(row, &["id"]).is_empty()
                        && uuid(util::text(row, &["upstreamId"]))
                        && row["catalogScope"] == "display_only"
                })
        })
}

pub fn cursor(data: &Value, seen: &mut BTreeSet<String>) -> Result<(Option<String>, Option<bool>)> {
    let Some(page) = data.get("page").filter(|page| !page.is_null()) else {
        return Ok((None, None));
    };
    if !page.is_object() {
        return Err(errors::protocol(ReadEndpoint::Models, "分页信息不是对象"));
    }
    match page["has_next_page"].as_bool() {
        Some(false) => Ok((None, Some(true))),
        Some(true) => {
            let cursor = util::text(page, &["cursor"]);
            if cursor.is_empty()
                || cursor.len() > 4096
                || cursor.chars().any(char::is_control)
                || !seen.insert(cursor.to_string())
            {
                return Err(errors::protocol(
                    ReadEndpoint::Models,
                    "下一页游标无效或重复，保留旧清单",
                ));
            }
            Ok((Some(cursor.to_string()), Some(false)))
        }
        None => Err(errors::protocol(ReadEndpoint::Models, "缺少分页完成标志")),
    }
}

pub fn too_large() -> Error {
    errors::protocol(
        ReadEndpoint::Models,
        "目录超过大小或分页上限，未发布不完整的新清单",
    )
}
