//! 官方客户端通过公共 Rainbow 配置取模型，不用 DescribeAIModelList 的空结果判不存在。
//! 按账号末两位选择同显示组的加权路由；真实模型名与思考字段随账号目录保存。

use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};
use std::collections::{BTreeMap, BTreeSet};

use super::{account, credentials::Credentials, upstream};

const CATALOG_URL: &str = "https://dnspod.cloud.tencent.com/ajax/rainbow?action=getRainbowConfig";
const GROUP: &str = "orcaterm.内置模型";
const TTL_MS: i64 = 300_000;
const DEFAULT: &str = "TokenHub/glm-5.3-flash";
const MODELS: [(&str, &str); 9] = [
    ("hy4-preview", "Hy4-preview"),
    ("hy3", "Hy3"),
    ("kimi-k3", "Kimi-K3"),
    ("glm-5.3-flash", "GLM-5.3-Flash"),
    ("glm-5.3", "GLM-5.3"),
    ("glm-5.2", "GLM-5.2"),
    ("deepseek-v4.1-flash", "DeepSeek-V4.1-Flash"),
    ("deepseek-v4-flash", "DeepSeek-V4-Flash"),
    ("deepseek-v4-pro", "DeepSeek-V4-Pro"),
];

fn base(id: &str, name: &str) -> Value {
    json!({ "id": id, "name": name, "supportsToolCall": false,
        "toolEmulation": { "protocol": "tagged_json_v1", "maxPromptChars": 190000 },
        "supportsImages": false, "supportsReasoning": false, "inputModalities": ["text"] })
}

pub fn list() -> Vec<Value> {
    MODELS.iter().map(|(id, name)| base(id, name)).collect()
}

pub fn snapshot() -> Result<Value> {
    let cached = sdk::catalog::cached()?;
    if cached["meta"]["remote"] == true && cached["models"].is_array() {
        return Ok(cached);
    }
    Ok(json!({ "models": list(), "meta": { "remote": false, "refreshed_at": 0 } }))
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|value| value.is_finite())
}
fn flag(value: &Value) -> bool {
    value == true || value == "1" || value == 1
}

fn levels(row: &Value) -> Result<Vec<Value>> {
    let levels = match &row["thinkingLevels"] {
        Value::Null => Vec::new(),
        Value::String(raw) if raw.trim().is_empty() => Vec::new(),
        Value::String(raw) => {
            serde_json::from_str(raw).map_err(|_| Error::http(502, "OrcaTerm 思考档位配置无效"))?
        }
        Value::Array(levels) => levels.clone(),
        _ => return Err(Error::http(502, "OrcaTerm 思考档位不是数组")),
    };
    if levels.len() > 32
        || levels.iter().any(|level| {
            level["value"]
                .as_str()
                .is_none_or(|value| value.is_empty() || value.len() > 64)
                || !level["requestParams"].is_object()
                || level["requestParams"].to_string().len() > 8192
        })
    {
        return Err(Error::http(502, "OrcaTerm 思考档位字段无效"));
    }
    Ok(levels)
}

fn parse(root: &Value, credentials: &Credentials) -> Result<Vec<Value>> {
    if root["code"] != 0 && root["code"] != "0" {
        return Err(Error::http(502, "OrcaTerm 公共模型配置请求失败"));
    }
    let rows = root["data"]
        .as_array()
        .and_then(|groups| groups.iter().find(|group| group["group"] == GROUP))
        .and_then(|group| group["rows"].as_array())
        .ok_or_else(|| Error::http(502, "OrcaTerm 配置缺少模型分组"))?;
    if rows.len() > 2048 {
        return Err(Error::http(502, "OrcaTerm 模型配置超过数量限制"));
    }
    let mut sorted: Vec<&Value> = rows
        .iter()
        .filter(|row| number(&row["weight"]).is_some_and(|weight| weight > 0.0))
        .collect();
    sorted.sort_by(|a, b| {
        number(&a["sort"])
            .unwrap_or(0.0)
            .total_cmp(&number(&b["sort"]).unwrap_or(0.0))
    });
    let mut groups: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for row in sorted {
        let wire = row["model"]
            .as_str()
            .filter(|wire| {
                !wire.trim().is_empty() && wire.len() <= 256 && !wire.chars().any(char::is_control)
            })
            .ok_or_else(|| Error::http(502, "OrcaTerm 有效模型缺少发送名称"))?;
        let group = row["text"]
            .as_str()
            .filter(|name| !name.is_empty())
            .unwrap_or(wire);
        groups.entry(group.to_string()).or_default().push(row);
    }
    let suffix: String = credentials
        .user_id
        .chars()
        .rev()
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let bucket = if suffix.is_empty() {
        0.0
    } else {
        suffix
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<f64>()
            .unwrap_or(f64::NAN)
            % 100.0
    };
    let mut chosen = Vec::new();
    for variants in groups.values() {
        let threshold = bucket / 100.0
            * variants
                .iter()
                .map(|row| number(&row["weight"]).unwrap_or(0.0))
                .sum::<f64>();
        let mut sum = 0.0;
        let selected = variants
            .iter()
            .copied()
            .find(|row| {
                sum += number(&row["weight"]).unwrap_or(0.0);
                threshold < sum
            })
            .or_else(|| variants.last().copied());
        if let Some(row) = selected {
            chosen.push(row);
        }
    }
    chosen.sort_by(|a, b| {
        number(&a["sort"])
            .unwrap_or(0.0)
            .total_cmp(&number(&b["sort"]).unwrap_or(0.0))
    });
    let mut ids = BTreeSet::new();
    let mut models = Vec::new();
    for row in chosen {
        let wire = row["model"].as_str().unwrap_or("");
        let id = wire.strip_prefix("TokenHub/").unwrap_or(wire);
        if id.is_empty() {
            return Err(Error::http(502, "OrcaTerm 有效模型缺少标识"));
        }
        if !ids.insert(id.to_ascii_lowercase()) {
            continue;
        }
        let name = row["displayName"]
            .as_str()
            .filter(|name| !name.is_empty())
            .or_else(|| row["text"].as_str().filter(|name| !name.is_empty()))
            .unwrap_or(id);
        let mut model = base(id, name);
        let levels = levels(row)?;
        let images = flag(&row["supportImg"]);
        model["supportsImages"] = json!(images);
        model["inputModalities"] = if images {
            json!(["text", "image"])
        } else {
            json!(["text"])
        };
        model["supportsReasoning"] = json!(flag(&row["showThinking"]) || !levels.is_empty());
        model["reasoningEffortLevels"] = json!(levels
            .iter()
            .filter_map(|level| level["value"].as_str())
            .collect::<Vec<_>>());
        model["orcatermNative"] = json!({ "model": wire, "enableThinking": flag(&row["showThinking"]), "levels": levels });
        models.push(model);
    }
    Ok(models)
}

pub fn refresh(force: bool) -> Result<Value> {
    let credentials = account::snapshot()?;
    if !force {
        let previous = snapshot()?;
        let age = sdk::now().saturating_sub(previous["meta"]["refreshed_at"].as_i64().unwrap_or(0));
        if previous["meta"]["remote"] == true
            && previous["models"].is_array()
            && (0..TTL_MS).contains(&age)
        {
            return Ok(sdk::catalog::unchanged(
                previous["models"].as_array().map(Vec::len).unwrap_or(0),
            ));
        }
    }
    // 公共配置不带账号Token、Cookie、用户内容或调用方请求头。
    let response = http::open_timeout(
        "POST",
        CATALOG_URL,
        json!({ "Content-Type": "application/json", "Accept": "application/json" }),
        Some(&json!({ "app": "ai", "groups": [GROUP], "options": { "enableEnv": 1 } })),
        15000,
    )?;
    let response = upstream::checked(response, "OrcaTerm 公共模型配置", &[])?;
    let bytes = response.bytes(1024 * 1024)?;
    let root: Value = serde_json::from_slice(&bytes)
        .map_err(|_| Error::http(502, "OrcaTerm 模型配置不是有效 JSON"))?;
    let models = parse(&root, &credentials)?;
    account::validate_current(&credentials)?;
    let payload = sdk::catalog::refreshed(models);
    if payload.to_string().len() > 256 * 1024 {
        return Err(Error::http(
            502,
            "OrcaTerm 模型目录超过账号快照限制，未覆盖旧目录",
        ));
    }
    Ok(payload)
}

pub fn selected(requested: &str) -> Result<Value> {
    let list = snapshot()?;
    let name = requested
        .trim()
        .strip_prefix("TokenHub/")
        .unwrap_or(requested.trim());
    let model = list["models"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|model| {
            model["id"] == name
                || model["name"] == requested
                || model["orcatermNative"]["model"] == requested
        });
    if let Some(model) = model {
        return Ok(model.clone());
    }
    if list["meta"]["remote"] == true {
        return Err(
            Error::http(403, "OrcaTerm 当前账号目录没有此模型，未静默换成其他模型")
                .with_code("account_model_unavailable"),
        );
    }
    let mut fallback = base(name, name);
    fallback["orcatermNative"]["model"] = json!(if MODELS.iter().any(|(id, _)| *id == name) {
        format!("TokenHub/{name}")
    } else {
        DEFAULT.to_string()
    });
    Ok(fallback)
}

pub fn settings(model: &Value, body: &Value) -> (bool, Value) {
    let native = &model["orcatermNative"];
    let levels = native["levels"].as_array().cloned().unwrap_or_default();
    let effort = body["reasoning_effort"]
        .as_str()
        .or_else(|| body["reasoning"]["effort"].as_str())
        .map(|effort| if effort == "none" { "off" } else { effort });
    let selected = effort
        .and_then(|effort| levels.iter().find(|level| level["value"] == effort))
        .or_else(|| levels.iter().find(|level| flag(&level["default"])))
        .or_else(|| levels.iter().find(|level| level["value"] != "off"))
        .or_else(|| levels.first());
    match selected {
        Some(level) => (level["value"] != "off", level["requestParams"].clone()),
        None => (native["enableThinking"] == true, json!({})),
    }
}
