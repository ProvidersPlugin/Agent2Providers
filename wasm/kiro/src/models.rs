//! 目录纯数据规则：API key/OAuth 使用同一模型形态，在线能力优先。
//! 自适应思考信息保存在每条模型上，避免宿主只保存 ModelList 时丢失顶层附加字段。

use crate::util;
use agent2api_wasm_sdk::{json, Error, Result, Value};

#[derive(Clone)]
pub struct NativeEffort {
    pub path: String,
    pub levels: Vec<String>,
    pub adaptive: bool,
}
pub fn thinks(model: &str) -> bool {
    model.eq_ignore_ascii_case("auto") || model.to_ascii_lowercase().contains("claude")
}
pub fn fallback() -> Vec<Value> {
    vec![
        json!({ "id": "auto", "name": "Auto", "supportsToolCall": true,
        "supportsReasoning": true, "isDefault": true, "kiroSource": "client-fallback" }),
    ]
}
fn native_schema(schema: &Value) -> Option<NativeEffort> {
    if !schema.is_object() {
        return None;
    }
    let adaptive = schema
        .pointer("/properties/thinking/properties/type/enum")
        .and_then(Value::as_array)
        .is_some_and(|values| values.iter().any(|value| value == "adaptive"));
    for path in ["output_config", "reasoning"] {
        if let Some(values) = schema
            .pointer(&format!("/properties/{path}/properties/effort/enum"))
            .and_then(Value::as_array)
        {
            let levels: Vec<_> = values
                .iter()
                .filter_map(Value::as_str)
                .filter(|level| util::identifier(level, 64))
                .take(20)
                .map(str::to_string)
                .collect();
            if !levels.is_empty() {
                return Some(NativeEffort {
                    path: path.into(),
                    levels,
                    adaptive,
                });
            }
        }
    }
    None
}
pub fn native(row: &Value) -> Option<NativeEffort> {
    let path = row["kiroNative"]["path"]
        .as_str()
        .filter(|path| matches!(*path, "output_config" | "reasoning"))?;
    let levels = row["kiroNative"]["levels"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    Some(NativeEffort {
        path: path.into(),
        levels,
        adaptive: row["kiroNative"]["adaptive"].as_bool().unwrap_or(true),
    })
}
pub fn parse(payload: &Value) -> Result<Vec<Value>> {
    let rows = payload["models"]
        .as_array()
        .ok_or_else(|| Error::http(502, "Kiro 目录缺少模型清单"))?;
    let default = payload["defaultModel"]["modelId"].as_str().unwrap_or("");
    if rows.len() > 2048 {
        return Err(Error::http(502, "Kiro 模型清单超过上限"));
    }
    let mut models = Vec::new();
    for row in rows {
        let Some(id) = row["modelId"]
            .as_str()
            .filter(|id| util::identifier(id, 256))
        else {
            continue;
        };
        if models.iter().any(|entry: &Value| {
            entry["id"]
                .as_str()
                .is_some_and(|old| old.eq_ignore_ascii_case(id))
        }) {
            continue;
        }
        let name = row["modelName"]
            .as_str()
            .filter(|name| !name.is_empty())
            .unwrap_or(if id == "auto" { "Auto" } else { id });
        let mut model = json!({ "id": id, "name": util::bounded(name, 400), "supportsToolCall": true,
            "supportsReasoning": thinks(id), "isDefault": if default.is_empty() { id == "auto" } else { id == default },
            "kiroSource": "remote" });
        for key in ["maxInputTokens", "maxOutputTokens"] {
            if let Some(tokens) = row["tokenLimits"][key]
                .as_i64()
                .filter(|tokens| *tokens > 0)
            {
                model[key] = json!(tokens);
            }
        }
        if let Some(types) = row["supportedInputTypes"].as_array() {
            model["supportsImages"] = json!(types.iter().any(|value| value
                .as_str()
                .is_some_and(|kind| kind.eq_ignore_ascii_case("IMAGE"))));
        }
        if let Some(native) = native_schema(&row["additionalModelRequestFieldsSchema"]) {
            model["supportsReasoning"] = json!(true);
            model["reasoningEffortLevels"] = json!(native.levels);
            model["kiroNative"] = json!({ "path": native.path, "levels": native.levels, "adaptive": native.adaptive });
        }
        if model["isDefault"] == true {
            models.insert(0, model);
        } else {
            models.push(model);
        }
    }
    if !rows.is_empty() && models.is_empty() {
        return Err(Error::http(502, "Kiro 模型清单全部无效，未覆盖上次目录"));
    }
    Ok(models)
}
pub fn saved(value: &Value) -> Option<Vec<Value>> {
    let rows = value.as_array()?;
    if rows.len() > 2048
        || !rows.iter().all(|row| {
            row["id"]
                .as_str()
                .is_some_and(|id| util::identifier(id, 256))
        })
    {
        return None;
    }
    Some(rows.clone())
}
