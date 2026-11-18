//! 账号相关的远程清单，无静态猜测表；宿主按 provider + account 持久化。
//! 原生实测目录只给组织/模型 ID，不包含思考能力或上下文，保持命名空间与工具位。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

use super::{client::Client, images, Region};

const TTL_MS: i64 = 10 * 60 * 1000;

fn empty() -> Value {
    json!({ "models": [], "meta": { "remote": false, "refreshed_at": 0 } })
}

pub fn list(params: &Value) -> Result<Value> {
    // 全局目录由宿主合并本家账号快照。无账号时不能访问、借用别人的目录。
    if params["account"].as_str().unwrap_or("").is_empty() {
        return Ok(empty());
    }
    cached()
}

fn cached() -> Result<Value> {
    let value = sdk::catalog::cached()?;
    Ok(if value["models"].is_array() && value["meta"].is_object() {
        value
    } else {
        empty()
    })
}

fn outcome(mut list: Value, refreshed: bool, message: Option<String>) -> Value {
    list["refreshed"] = json!(refreshed);
    list["count"] = json!(list["models"].as_array().map(Vec::len).unwrap_or(0));
    if let Some(message) = message {
        list["message"] = json!(message);
    }
    list
}

pub fn refresh(region: Region, params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        // 主程序会为每个账号单独调度；尚未加号不是上游失败。
        return Ok(outcome(empty(), false, None));
    }
    let previous = cached()?;
    let api = Client::for_account(region)?;
    if params["force"].as_bool() != Some(true) {
        let age = sdk::now().saturating_sub(previous["meta"]["refreshed_at"].as_i64().unwrap_or(0));
        if previous["meta"]["remote"] == true && (0..TTL_MS).contains(&age) {
            api.check()?;
            return Ok(outcome(previous, false, None));
        }
    }
    let payload = match api.models_json() {
        Ok(payload) => payload,
        Err(error) if matches!(error.code.as_deref(), Some("account_changed" | "forbidden")) => {
            return Err(error)
        }
        Err(error) => return Ok(outcome(previous, false, Some(error.message))),
    };
    let rows = payload
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| payload.as_array());
    let Some(rows) = rows else {
        return Ok(outcome(
            previous,
            false,
            Some("ModelScope 模型目录格式无法识别（保留现有清单）".to_string()),
        ));
    };
    if rows.len() > 4096
        || rows.iter().any(|row| {
            row["id"].as_str().is_none_or(|id| {
                id.trim().is_empty() || id.len() > 512 || id.chars().any(char::is_control)
            })
        })
    {
        return Ok(outcome(
            previous,
            false,
            Some("ModelScope 模型目录条目无效或超过限制（保留现有清单）".to_string()),
        ));
    }
    let mut models = Vec::with_capacity(rows.len());
    for row in rows {
        let id = row["id"]
            .as_str()
            .ok_or_else(|| Error::http(502, "ModelScope 模型名称无效"))?
            .trim();
        if models
            .iter()
            .any(|model: &Value| model["id"].as_str() == Some(id))
        {
            continue;
        }
        let mut model = json!({ "id": id, "name": id, "supportsToolCall": true });
        images::decorate(region, id, &mut model);
        models.push(model);
    }
    let refreshed = sdk::catalog::refreshed(models);
    if refreshed.to_string().len() > 256 * 1024 {
        return Ok(outcome(
            previous,
            false,
            Some("ModelScope 模型目录超过宿主限额（保留现有清单）".to_string()),
        ));
    }
    api.check()?;
    Ok(refreshed)
}
