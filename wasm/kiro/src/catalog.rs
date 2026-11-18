//! 按账号隔离的持久目录，十分钟 TTL + 失败短退避，普通展示目录不是账号硬权限表。
//! OAuth GET；API key AWS JSON 分页。拒绝循环 nextToken 或截断后假称完整目录。

use crate::{credentials::Credentials, endpoints, errors, models, network, refresh, util};
use agent2api_wasm_sdk::{
    json,
    state::{self, Scope, Snapshot},
    AccountLock, Error, Result, Value,
};

const KEY: &str = "kiro.catalog";
const TTL: i64 = 600_000;
const BACKOFF: i64 = 30_000;
fn list_value(models: Vec<Value>, remote: bool, at: i64) -> Value {
    json!({ "models": models, "meta": { "remote": remote, "refreshed_at": at } })
}
pub fn fallback() -> Value {
    list_value(models::fallback(), false, 0)
}
fn binding(c: &Credentials) -> Result<String> {
    Ok(util::hash(
        &format!(
            "{}:{}:{}:{}",
            c.method,
            c.runtime_region()?,
            c.profile,
            if c.api_key() {
                util::hash(&c.access, 16)
            } else {
                format!(
                    "{}:{}",
                    crate::credentials::identity(c)?,
                    c.account_id.to_ascii_lowercase()
                )
            }
        ),
        16,
    ))
}
fn put(old: &Snapshot, value: Value) -> Result<()> {
    if !old.compatible {
        return Err(Error::http(409, "Kiro 目录状态版本不兼容，未覆盖原状态"));
    }
    if value.to_string().len() > 256 * 1024 {
        return Err(Error::http(502, "Kiro 模型目录超过持久状态上限"));
    }
    if !state::compare_exchange(Scope::Account, KEY, 1, old.revision, value, 0)? {
        return Err(Error::http(409, "Kiro 目录被另一操作更新，请重试"));
    }
    Ok(())
}
fn fetch(c: &Credentials) -> Result<Vec<Value>> {
    let base = endpoints::management(&c.runtime_region()?)?;
    if !c.api_key() {
        let mut url = format!("{base}/List-Available-Models?origin=KIRO_CLI");
        if !c.profile.is_empty() {
            url.push_str(&format!("&profileArn={}", util::query(&c.profile)));
        }
        let page = network::get(c, &url)?;
        if page
            .get("nextToken")
            .is_some_and(|next| !next.is_null() && next != "")
        {
            return Err(Error::http(
                502,
                "Kiro OAuth 目录仍有未完成分页，未覆盖上次清单",
            ));
        }
        return models::parse(&page);
    }
    let mut rows = Vec::new();
    let mut default = Value::Null;
    let mut seen = Vec::new();
    let mut next = String::new();
    for index in 0..20 {
        let mut body = json!({ "origin": "KIRO_CLI" });
        if !next.is_empty() {
            body["nextToken"] = json!(next);
        }
        let page = network::aws_json(
            c,
            &format!("{base}/?origin=KIRO_CLI"),
            endpoints::MODELS_TARGET,
            &body,
        )?;
        let page_rows = page["models"]
            .as_array()
            .ok_or_else(|| Error::http(502, "Kiro 模型分页缺少清单"))?;
        rows.extend(page_rows.iter().cloned());
        if rows.len() > 2048 {
            return Err(Error::http(502, "Kiro 模型数量超过上限"));
        }
        if default.is_null() {
            default = page["defaultModel"].clone();
        }
        next = match page.get("nextToken") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(token)) => token.clone(),
            _ => return Err(Error::http(502, "Kiro模型分页游标格式无效，未覆盖上次目录")),
        };
        if next.is_empty() {
            return models::parse(&json!({ "models": rows, "defaultModel": default }));
        }
        if next.len() > 8192 || seen.contains(&next) || index == 19 {
            return Err(Error::http(502, "Kiro 模型分页未完整结束，未覆盖上次目录"));
        }
        seen.push(next.clone());
    }
    Err(Error::http(502, "Kiro 模型目录不完整"))
}
fn load(c: &Credentials, force: bool) -> Result<Value> {
    let _lock = AccountLock::acquire()?;
    let snapshot = state::get(Scope::Account, KEY, 1)?;
    let bound = binding(c)?;
    let same = snapshot.compatible && snapshot.value["binding"] == bound;
    let old = if same {
        snapshot.value["list"].clone()
    } else {
        fallback()
    };
    let previous = if models::saved(&old["models"]).is_some() {
        old
    } else {
        fallback()
    };
    let now = util::now()?;
    let age = now.saturating_sub(previous["meta"]["refreshed_at"].as_i64().unwrap_or(0));
    if !force && same && previous["meta"]["remote"] == true && (0..TTL).contains(&age) {
        let mut value = previous;
        let host = agent2api_wasm_sdk::catalog::cached()?;
        // 请求链可能先刷新了插件状态，但宿主只持久化 models.refresh 的成功回包。
        // 第一次正式刷新仍须交付该同账号快照，否则宿主权限一直是未知。
        value["refreshed"] = json!(
            host["meta"]["remote"] != true
                || host["models"] != value["models"]
                || host["meta"]["refreshed_at"] != value["meta"]["refreshed_at"]
        );
        return Ok(value);
    }
    if !force
        && same
        && snapshot.value["retry_at"]
            .as_i64()
            .is_some_and(|at| at > now)
    {
        let mut value = previous;
        value["refreshed"] = json!(false);
        value["message"] = json!("Kiro目录暂不可用，保留上次清单，稍后重试");
        return Ok(value);
    }
    match fetch(c) {
        Ok(models) => {
            let list = list_value(models, true, now);
            put(
                &snapshot,
                json!({ "binding": bound, "list": list, "retry_at": 0 }),
            )?;
            let mut list = list;
            list["refreshed"] = json!(true);
            Ok(list)
        }
        Err(error) if errors::host_stop(&error) || error.status == Some(401) => Err(error),
        Err(error) => {
            put(
                &snapshot,
                json!({ "binding": bound, "list": previous, "retry_at": now.saturating_add(BACKOFF) }),
            )?;
            let mut list = previous;
            list["refreshed"] = json!(false);
            list["message"] = json!(errors::safe_message(&error.message, &c.secrets()));
            Ok(list)
        }
    }
}
fn ready_list(force: bool) -> Result<Value> {
    let session = refresh::ready()?;
    match load(&session.credentials, force) {
        Err(error) if error.status == Some(401) && !session.credentials.api_key() => load(
            &refresh::after_rejection(&session.credentials)?.credentials,
            force,
        ),
        result => result,
    }
}
pub fn list(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        // 全局展示/账号聚合由宿主处理；账号目录能力不允许无账号读取其他人的缓存。
        return Ok(fallback());
    }
    ready_list(false)
}
pub fn refresh(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        let mut value = list(params)?;
        value["refreshed"] = json!(false);
        value["count"] = json!(value["models"].as_array().map(Vec::len).unwrap_or(0));
        return Ok(value);
    }
    let mut value = ready_list(params["force"].as_bool().unwrap_or(false))?;
    value["refreshed"] = json!(value["refreshed"] == true);
    value["count"] = json!(value["models"].as_array().map(Vec::len).unwrap_or(0));
    Ok(value)
}
pub fn for_chat(c: &Credentials, id: &str) -> Result<Value> {
    let list = load(c, false)?;
    if let Some(model) = list["models"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|model| {
            model["id"]
                .as_str()
                .is_some_and(|model| model.eq_ignore_ascii_case(id))
        })
    {
        return Ok(model.clone());
    }
    // 宿主账号目录负责选路；当前账号拿到真实清单后不再假报缺失型号可用。
    if list["meta"]["remote"] == true {
        return Err(
            Error::http(400, "Kiro 当前账号目录没有此模型，请刷新目录或选择可用型号")
                .with_code("account_model_unavailable"),
        );
    }
    Ok(json!({ "id": id, "supportsReasoning": models::thinks(id), "supportsToolCall": true }))
}
