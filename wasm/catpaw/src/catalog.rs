//! 每个账号使用宿主持久化的独立目录；静态表只供缺缓存时展示，不代替原账号授权。
//! models.list 只读缓存；models.refresh 才请求上游。失败不覆盖最后一份成功清单。

use agent2api_wasm_sdk::{self as sdk, json, Result, Value};

use crate::{credentials, errors, models, network, util};

const TTL_MS: i64 = 15 * 60 * 1000;

fn displayed() -> Result<Value> {
    let value = sdk::catalog::cached().map_err(errors::host)?;
    Ok(models::cached(&value)?.unwrap_or_else(models::fallback))
}

pub fn list(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() { return Ok(models::fallback()); }
    displayed()
}

fn outcome(mut list: Value, refreshed: bool, message: Option<&str>) -> Value {
    list["refreshed"] = json!(refreshed);
    list["count"] = json!(list["models"].as_array().map(Vec::len).unwrap_or(0));
    if let Some(message) = message { list["message"] = json!(message); }
    list
}

pub fn refresh(params: &Value) -> Result<Value> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        return Ok(outcome(models::fallback(), false, Some("没有指定 CatPaw 账号，未读取本机来源或请求上游；保留已有展示目录")));
    }
    let previous = displayed();
    if let Err(error) = &previous {
        if errors::host_stop(error) { return Err(error.clone()); }
    }
    // 先校验当前凭据：desktop-auth 即便有缓存也不能伪装成已连接的本机来源。
    let credentials = credentials::current()?;
    credentials.require_main()?;
    let now = util::now()?;
    // 本机引用的同一存储身份会随官方客户端切号；不能让上一位用户的目录 TTL
    // 阻止查询当前登录账号，即使此前目录仍在显示中。
    let desktop = sdk::account().map_err(errors::host)?["desktop"] == true;
    if params["force"] != true && !desktop {
        if let Ok(previous) = &previous {
            let age = now.checked_sub(previous["meta"]["refreshed_at"].as_i64().unwrap_or(0));
            if previous["meta"]["remote"] == true && age.is_some_and(|age| (0..TTL_MS).contains(&age)) {
                return Ok(outcome(previous.clone(), false, None));
            }
        }
    }
    let fetched = network::catalog(&credentials)
        .and_then(|payload| models::parse(&payload, util::now()?, &credentials.secrets()));
    credentials::check_current(&credentials)?;
    match fetched {
        Ok(list) => Ok(outcome(list, true, None)),
        Err(error) if errors::host_stop(&error) || error.status == Some(401)
            || error.code.as_deref() == Some("catpaw_catalog_conflict") => Err(error),
        Err(error) => match previous {
            Ok(previous) => Ok(outcome(previous, false, Some(&error.message))),
            Err(_) => Err(error),
        },
    }
}
