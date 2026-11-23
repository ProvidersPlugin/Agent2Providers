//! 旧后台探测迁为 tasks.v1 受控任务；无线程/全局表，6小时结果缓存用命名 conversation。
//! 每次任务只探最多4个模型，跨调用保存进度；全拒不清空目录，未知结论沿用旧标记。
//! 81009=推理端未部署（保留）；81004/81010/81016=权限拒绝（展示隐藏）。
//! 宿主当前未消费 tasks.run 的目录变更，结论在下一次 models.list/refresh 应用。

use super::{account, client, credentials::Credentials, errors, models};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Snapshot},
    AccountLock, Error, Result, Value,
};

const SCOPE: &str = "model-probe";
const KEY: &str = "snapshot";
const TTL: i64 = 6 * 3600 * 1000;

fn load() -> Result<Snapshot> {
    let state = state::conversation_get(SCOPE, KEY, 1)?;
    if !state.compatible {
        return Err(Error::new("OfficeAce 模型探测状态格式已变化"));
    }
    Ok(state)
}
fn put(snapshot: &Snapshot, value: Value) -> Result<bool> {
    state::conversation_compare_exchange(
        SCOPE,
        KEY,
        1,
        snapshot.revision,
        value,
        TTL + 30 * 60 * 1000,
    )
}
fn contains(rows: &Value, id: &str) -> bool {
    rows.as_array().into_iter().flatten().any(|value| {
        value
            .as_str()
            .is_some_and(|value| value.eq_ignore_ascii_case(id))
    })
}

pub fn request(credential: &Credentials) -> Result<()> {
    for _ in 0..3 {
        let state = load()?;
        if state.value["binding"] == models::binding(credential) && state.value["job"].is_object() {
            return Ok(());
        }
        let mut value = if state.value["binding"] == models::binding(credential) {
            state.value.clone()
        } else {
            json!({ "binding": models::binding(credential) })
        };
        value["requested"] = json!(true);
        if put(&state, value)? {
            return Ok(());
        }
    }
    Err(Error::http(409, "模型探测状态已变化，未覆盖已有任务"))
}

pub fn apply(rows: &mut Vec<Value>, credential: &Credentials) -> Result<()> {
    let snapshot = load()?;
    if snapshot.value["binding"] != models::binding(credential) {
        return Ok(());
    }
    rows.retain(|row| !contains(&snapshot.value["hidden"], row["id"].as_str().unwrap_or("")));
    for row in rows {
        let undeployed = contains(
            &snapshot.value["undeployed"],
            row["id"].as_str().unwrap_or(""),
        );
        if let Some(object) = row.as_object_mut() {
            object.remove("upstreamStatus");
        }
        if undeployed {
            row["upstreamStatus"] = json!("undeployed");
        }
    }
    Ok(())
}

pub fn run(params: &Value) -> Result<Value> {
    let (record, credential) = account::snapshot()?;
    let _lock = AccountLock::acquire_named_conversation(SCOPE)?;
    account::same_model(&record)?;
    let stored = models::stored(&credential)?;
    let ids: Vec<String> = stored["catalog"]["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| row["id"].as_str().map(str::to_string))
        .collect();
    if ids.is_empty() {
        return Ok(sdk::tasks::result(
            true,
            "尚无真实账号目录，本轮未发探测请求",
        ));
    }
    let initial = load()?;
    let mut value = if initial.value["binding"] == models::binding(&credential) {
        initial.value.clone()
    } else {
        json!({ "binding": models::binding(&credential), "hidden": [], "undeployed": [], "at": 0 })
    };
    if value.get("at").is_none() {
        value["at"] = json!(0);
    }
    if value["blockedAt"].as_i64().is_some_and(|at| {
        at > 0 && params["manual"] != true && sdk::now().saturating_sub(at) < 30 * 60 * 1000
    }) {
        return Ok(sdk::tasks::result(
            false,
            "账号服务器来源或代理设置尚未满足，请检查账号凭据并使用直连；已暂停探测",
        ));
    }
    let age = sdk::now().saturating_sub(value["at"].as_i64().unwrap_or(0));
    if !value["job"].is_object()
        && value["requested"] != true
        && params["manual"] != true
        && (0..TTL).contains(&age)
        && value["at"] != 0
    {
        return Ok(sdk::tasks::result(true, "模型可用性结论仍在有效期内"));
    }
    if value["job"]["ids"] != json!(ids) {
        value["job"] = json!({ "ids": ids, "offset": 0, "results": [] });
    }
    // 发探测前先持久当前任务，目录反复刷新会看到在途状态，不重复启动整轮。
    if !put(&initial, value.clone())? {
        return Err(Error::http(409, "模型探测任务已被更新"));
    }
    let mut before = load()?;
    let start = value["job"]["offset"].as_u64().unwrap_or(0) as usize;
    let end = start.saturating_add(4).min(ids.len());
    for (offset, id) in ids[start.min(ids.len())..end].iter().enumerate() {
        // 在请求前保留unknown占位并前移进度；宿主取消/超时不重放这个可能已消费的探测。
        if let Some(results) = value["job"]["results"].as_array_mut() {
            results.push(json!({ "id": id, "result": "unknown" }));
        }
        value["job"]["offset"] = json!(start + offset + 1);
        if !put(&before, value.clone())? {
            return Err(Error::http(409, "模型探测进度已变化"));
        }
        before = load()?;
        account::same_model(&record)?;
        let result = match one(&credential, id) {
            Ok(result) => result,
            Err(error) if errors::permission(&error) => {
                value["blockedAt"] = json!(sdk::now());
                let _ = put(&before, value);
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        account::same_model(&record)?;
        if let Some(last) = value["job"]["results"]
            .as_array_mut()
            .and_then(|results| results.last_mut())
        {
            last["result"] = json!(result);
        }
        if !put(&before, value.clone())? {
            return Err(Error::http(409, "模型探测结果已被更新"));
        }
        before = load()?;
    }
    value["blockedAt"] = json!(0);
    value["job"]["offset"] = json!(end);
    if end == ids.len() {
        let results = value["job"]["results"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut hidden = Vec::new();
        let mut undeployed = Vec::new();
        for result in &results {
            let id = result["id"].as_str().unwrap_or("");
            match result["result"].as_str() {
                Some("hidden") => hidden.push(json!(id)),
                Some("undeployed") => undeployed.push(json!(id)),
                Some("unknown") => {
                    if contains(&value["hidden"], id) {
                        hidden.push(json!(id));
                    }
                    if contains(&value["undeployed"], id) {
                        undeployed.push(json!(id));
                    }
                }
                _ => {}
            }
        }
        if results.iter().all(|row| row["result"] == "hidden") {
            hidden.clear();
        }
        value["hidden"] = json!(hidden);
        value["undeployed"] = json!(undeployed);
        value["job"] = Value::Null;
        value["at"] = json!(sdk::now());
        value["requested"] = json!(false);
    }
    account::same_model(&record)?;
    if !put(&before, value)? {
        return Err(Error::http(409, "模型探测状态已被更新，未覆盖新结论"));
    }
    Ok(sdk::tasks::result(
        true,
        &if end == ids.len() {
            "模型探测已完成，下一次目录读取或刷新将应用结果".into()
        } else {
            format!("模型探测已处理 {end}/{}，后续调度继续", ids.len())
        },
    ))
}

fn one(credential: &Credentials, id: &str) -> Result<&'static str> {
    let body =
        json!({ "model": id, "max_tokens": 1, "messages": [{ "role": "user", "content": "x" }] });
    let response = match client::model(
        "POST",
        "/v2/chat/completions",
        credential,
        Some(&body),
        json!({ "accept": "application/json" }),
    ) {
        Ok(response) => response,
        Err(error) if errors::permission(&error) => return Err(error),
        Err(_) => return Ok("unknown"),
    };
    let status = response.status;
    // 旧探测只用HTTP接受确认可达；不消费完整回答，不把探测输出当用户答案。
    if (200..300).contains(&status) {
        let json_response = response.header_values("content-type").iter().any(|mime| {
            let mime = mime
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            mime == "application/json" || mime.ends_with("+json")
        });
        if !json_response {
            return Ok("available");
        }
        // 伪 200 错误不是可达成功；只读有界错误正文，不消费完整推理流。
    }
    if status == 401 || status == 429 || status >= 500 {
        return Ok("unknown");
    }
    let bytes = match response.bytes(256 * 1024) {
        Ok(bytes) => bytes,
        Err(_) => return Ok("unknown"),
    };
    let lower = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
    if lower.contains("apig.") {
        return Ok("unknown");
    }
    Ok(
        if ["81009", "invalid model"]
            .iter()
            .any(|part| lower.contains(part))
        {
            "undeployed"
        } else if ["81004", "81010", "81016", "insufficient permission"]
            .iter()
            .any(|part| lower.contains(part))
        {
            "hidden"
        } else {
            "unknown"
        },
    )
}

pub fn tasks() -> Value {
    json!([{ "id": "model-probe", "label": "OfficeAce 模型可用性探测", "enabled": true,
        "times": [], "interval_ms": 60_000, "utc_offset_minutes": 480, "checkin": false }])
}
