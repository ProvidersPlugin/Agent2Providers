//! 公共目录+企业目录均来自当前账号，企业权限单独绑定身份30分钟；展示不等于授权。
//! 不启用宿主catalog.accounts强闸门，保持原生个人自定义映射语义，企业在包内拒绝未知权限。
use super::{
    account, builtin, endpoints, headers, image_models, network, reasoning,
    session::{self, Session},
    util,
};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    Error, Result, Value,
};

const KEY: &str = "model-catalog";
const LEDGER: &str = "workbuddy-model-permissions";
const TTL: i64 = 30 * 60_000;
pub fn fallback() -> Value {
    let mut models = builtin::builtin_filtered();
    for model in &mut models {
        reasoning::annotate(model);
    }
    json!({ "models": models, "meta": { "remote": false, "refreshed_at": 0 } })
}
fn stored(current: &Session) -> Result<Option<Value>> {
    let snapshot = state::get(Scope::Account, KEY, 1)?;
    if !snapshot.compatible {
        return Err(Error::http(409, "WorkBuddy目录状态格式已变化"));
    }
    if snapshot.value["authority"] == account::authority(&current.secret)
        && snapshot.value["catalog"]["models"].is_array()
    {
        return Ok(Some(snapshot.value));
    }
    // 接管原生同provider台账；仅同一授权身份且确有上游fetchedAt时恢复，不借用国内槽。
    let legacy = sdk::host("ledger.get", json!({ "kind": LEDGER }))?;
    if legacy["authority"] == account::authority(&current.secret)
        && legacy["models"].is_array()
        && legacy["fetchedAt"]
            .as_i64()
            .is_some_and(|at| at > 0 && at <= sdk::now())
    {
        let value = json!({ "authority": legacy["authority"], "complete": legacy["complete"], "fetchedAt": legacy["fetchedAt"],
            "catalog": { "models": legacy["models"], "meta": { "remote": true, "refreshed_at": legacy["fetchedAt"] } } });
        if state::compare_exchange(Scope::Account, KEY, 1, snapshot.revision, value.clone(), 0)? {
            return Ok(Some(value));
        }
    }
    Ok(None)
}
pub fn list(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        let cached = sdk::catalog::cached()?;
        return Ok(
            if cached["models"].is_array() && cached["meta"]["remote"] == true {
                cached
            } else {
                fallback()
            },
        );
    }
    let current = session::snapshot()?;
    Ok(stored(&current)?
        .map(|v| v["catalog"].clone())
        .filter(|v| v["models"].is_array() && v["meta"]["remote"] == true)
        .unwrap_or_else(fallback))
}
pub fn known(id: &str) -> Option<Value> {
    let catalog = sdk::catalog::cached()
        .ok()
        .filter(|v| v["models"].is_array() && v["meta"]["remote"] == true)
        .unwrap_or_else(fallback);
    catalog["models"]
        .as_array()?
        .iter()
        .find(|row| {
            row["id"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(id))
        })
        .cloned()
}
fn parse(payload: &Value, enterprise: bool) -> Result<Vec<Value>> {
    let rows = if enterprise {
        payload.get("data")
    } else {
        payload.pointer("/data/models")
    }
    .and_then(Value::as_array)
    .ok_or_else(|| Error::http(502, "WorkBuddy目录缺少模型数组"))?;
    let agents = &payload["data"]["agents"];
    let cli = agents
        .as_array()
        .and_then(|rows| rows.iter().find(|v| v["name"] == "cli"))
        .or_else(|| agents.get("cli"));
    let picker = cli.and_then(|v| v["models"].as_array());
    let mut models = Vec::new();
    for row in rows {
        if !row.is_object()
            || row["disabled"] == true
            || !row["id"].as_str().is_some_and(|v| !v.is_empty())
        {
            continue;
        }
        if enterprise
            && !row["tags"]
                .as_array()
                .is_some_and(|v| v.iter().any(|v| v == "chat"))
            && image_models::capabilities(row).is_none()
        {
            continue;
        }
        let mut model = if let Some(image) = image_models::decorate(row) {
            image
        } else if builtin::is_chat_model(row) {
            row.clone()
        } else {
            continue;
        };
        let id = row["id"].as_str().unwrap_or("");
        let auto = id.eq_ignore_ascii_case("auto");
        if model["kind"] != "image" {
            model["isDefault"] = json!(auto);
            model["modelType"] = json!(if auto {
                "auto"
            } else if enterprise {
                "custom"
            } else {
                "built-in"
            });
            if let Some(picker) = picker.filter(|_| !enterprise) {
                model["visibility"] = json!(if picker
                    .iter()
                    .any(|v| v.as_str().is_some_and(|v| v.eq_ignore_ascii_case(id)))
                {
                    "shown"
                } else {
                    "hidden"
                });
            }
        }
        model["workbuddyAccountScoped"] = json!(enterprise);
        reasoning::annotate(&mut model);
        models.push(model);
    }
    Ok(models)
}
fn fetch(current: &Session, path: &str, enterprise: bool) -> Result<Vec<Value>> {
    let reply = network::raw_timeout(
        "GET",
        &current.endpoint()?,
        path,
        headers::framework(&current.secret)?,
        None,
        if enterprise { 5000 } else { 20_000 },
    )?;
    if !reply.ok() {
        return Err(super::errors::upstream(
            reply.status,
            &reply.value,
            &current.secrets(),
        ));
    }
    parse(&reply.value, enterprise)
}
pub fn refresh(params: &Value) -> Result<Value> {
    let current = session::ensure(false)?;
    refresh_for(&current).or_else(|error| {
        let mut old = list(params)?;
        old["refreshed"] = json!(false);
        old["count"] = json!(old["models"].as_array().map(Vec::len).unwrap_or(0));
        old["message"] = json!(error.message);
        Ok(old)
    })
}
pub fn refresh_for(current: &Session) -> Result<Value> {
    let snapshot = state::get(Scope::Account, KEY, 1)?;
    if !snapshot.compatible {
        return Err(Error::http(409, "WorkBuddy目录状态格式已变化"));
    }
    let primary = fetch(current, "/v3/config", false);
    let enterprise = !account::personal(&current.secret);
    let secondary = if enterprise {
        Some(fetch(
            current,
            &format!(
                "/console/enterprises/{}/config/models",
                endpoints::encode(&util::text(&current.secret, &["enterpriseId"]))
            ),
            true,
        ))
    } else {
        None
    };
    let complete = primary.is_ok() && secondary.as_ref().is_none_or(Result::is_ok);
    let mut models = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut failures = Vec::new();
    for result in std::iter::once(primary).chain(secondary) {
        match result {
            Ok(rows) => {
                for row in rows {
                    if seen.insert(util::text(&row, &["id"]).to_ascii_lowercase()) {
                        models.push(row);
                    }
                }
            }
            Err(e) => failures.push(e.message),
        }
    }
    if !complete {
        // 目录刷新失败不覆盖最后成功快照，尤其不能把未读到的企业条目当成撤权。
        return Err(Error::http(502, failures.join("；")));
    }
    session::check(current)?;
    let mut catalog = sdk::catalog::refreshed(models.clone());
    if !failures.is_empty() {
        catalog["warnings"] = json!(failures);
    }
    let value = json!({ "authority": account::authority(&current.secret), "complete": complete, "catalog": catalog, "fetchedAt": sdk::now() });
    if !state::compare_exchange(Scope::Account, KEY, 1, snapshot.revision, value, 0)? {
        return Err(Error::http(409, "WorkBuddy目录已被其他请求更新"));
    }
    let _ = sdk::host(
        "ledger.put",
        json!({ "kind": LEDGER, "value": { "authority": account::authority(&current.secret), "models": models, "complete": complete, "fetchedAt": sdk::now() } }),
    );
    // 宿主展示目录是提供商级合并视图。企业新条目必须标记账号限定，不能据此授权个人号。
    let previous = sdk::catalog::cached()
        .ok()
        .filter(|v| v["models"].is_array() && v["meta"]["remote"] == true)
        .unwrap_or_else(fallback);
    // 公共配置由本次成功响应替换，企业条目按来源标记，旧其它企业条目仅供展示。
    // 不能把企业账号也能看到的公共模型整体标成企业专属，否则个人账号会被误拦。
    let mut display = models.clone();
    for model in previous["models"].as_array().into_iter().flatten() {
        let id = util::text(model, &["id"]);
        if model["workbuddyAccountScoped"] == true
            && !display
                .iter()
                .any(|row| util::text(row, &["id"]).eq_ignore_ascii_case(&id))
        {
            display.push(model.clone());
        }
    }
    catalog["models"] = json!(display);
    catalog["count"] = json!(display.len());
    Ok(catalog)
}
pub fn image(current: &Session, model: &str) -> Result<Value> {
    let mut own = stored(current)?;
    if own.as_ref().is_none_or(|v| {
        v["fetchedAt"]
            .as_i64()
            .is_none_or(|at| at > sdk::now() || sdk::now().saturating_sub(at) >= TTL)
    }) {
        refresh_for(current).map_err(|_| Error::http(503, "国际图片目录尚未确认，未发出请求"))?;
        own = stored(current)?;
    }
    own.as_ref()
        .and_then(|v| v["catalog"]["models"].as_array())
        .and_then(|rows| {
            rows.iter().find(|row| {
                row["id"]
                    .as_str()
                    .is_some_and(|id| id.eq_ignore_ascii_case(model))
                    && image_models::capabilities(row).is_some()
            })
        })
        .cloned()
        .ok_or_else(|| {
            Error::http(403, "当前国际账号目录没有此图片模型")
                .with_code("account_model_unavailable")
        })
}

pub fn check(current: &Session, model: &str) -> Result<Option<Value>> {
    if account::personal(&current.secret) {
        let own = stored(current)?;
        if let Some(row) = own
            .as_ref()
            .and_then(|v| v["catalog"]["models"].as_array())
            .and_then(|rows| {
                rows.iter().find(|v| {
                    v["id"]
                        .as_str()
                        .is_some_and(|id| id.eq_ignore_ascii_case(model))
                })
            })
        {
            return Ok(Some(row.clone()));
        }
        let model = known(model);
        if model
            .as_ref()
            .is_some_and(|v| v["workbuddyAccountScoped"] == true)
        {
            return Err(Error::http(403, "该模型只在企业目录声明，个人账号未获授权")
                .with_code("account_model_unavailable"));
        }
        return Ok(model);
    }
    let mut saved = stored(current)?;
    if saved.as_ref().is_none_or(|v| {
        v["fetchedAt"]
            .as_i64()
            .is_none_or(|at| at > sdk::now() || sdk::now() - at >= TTL)
    }) {
        refresh_for(current).map_err(|_| {
            Error::http(503, "企业模型权限未确认，未发出请求")
                .with_code("account_model_permissions_unknown")
        })?;
        saved = stored(current)?;
    }
    let saved = saved.ok_or_else(|| Error::http(503, "企业目录无法恢复"))?;
    if let Some(row) = saved["catalog"]["models"].as_array().and_then(|rows| {
        rows.iter().find(|v| {
            v["id"]
                .as_str()
                .is_some_and(|id| id.eq_ignore_ascii_case(model))
        })
    }) {
        return Ok(Some(row.clone()));
    }
    if saved["complete"] == true {
        Err(Error::http(403, "当前企业目录没有此模型").with_code("account_model_unavailable"))
    } else {
        Err(Error::http(503, "企业目录只取到部分条目，此模型权限未知")
            .with_code("account_model_permissions_unknown"))
    }
}
