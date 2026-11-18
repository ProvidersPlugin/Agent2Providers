//! Trae方式0仅消费宿主固定托管登录；原监听器/PKCE/密钥与一次换码完全留在宿主。
//! 不接手填URL/授权码、不查询账号、不构造OAuth状态；失败没有普通HTTP或备用源回退。

use agent2api_wasm_sdk::{self as sdk, json, Result, Value};

use crate::{i18n, login_stage as stage};

pub fn start(params: &Value) -> Result<Value> {
    let context = stage::context(params)?;
    let variant = stage::variant(params, &context)?;
    stage::recheck(params, &context)?;
    // 每次真实Start只申请一次原监听器上下文；失败不重bind或重新选择变体。
    let prepared = stage::prepare(&context, variant)?;
    stage::recheck(params, &context)?;
    let locale = stage::locale(params);
    let published = stage::publish(&context, &prepared, locale)?;
    // 发布改变了当前步骤，旧expected不再读取；原expiry不能续租或恢复旧发布材料。
    Ok(
        json!({ "flow": context.flow, "url": "", "poll_interval_ms": 1500,
        "external_browser": true, "allowed_hosts": [], "instructions": i18n::web(locale),
        "interaction": published }),
    )
}

pub fn callback(params: &Value) -> Result<Value> {
    let context = stage::context(params)?;
    if !params["payload"].is_null() {
        return Err(stage::invalid());
    }
    // expected只是原Call步骤索引。code/URL/PKCE/设备材料均不从params或state取值。
    // 真收据只由宿主兑换消费一次；异常、响应丢失或Wasm中断都不重新调用。
    let reply = sdk::host(
        "login.trae.exchange",
        json!({ "expected": context.interaction }),
    )?;
    let account = stage::account(reply)?;
    stage::recheck(params, &context)?;
    // 宿主保存前核对本次完整见证，不可重新normalize、读GetUserInfo或改写secret/public。
    Ok(sdk::login::done(account))
}

pub fn poll(params: &Value) -> Result<Value> {
    stage::context(params)?;
    if !params["payload"].is_null() {
        return Err(stage::invalid());
    }
    // 只读当前托管上下文，不发HTTP、不兑换、不重新发布浏览器步骤。
    Ok(json!({ "state": "pending" }))
}

pub fn cancel(params: &Value) -> Result<Value> {
    stage::flow(params)?;
    // 本片无可恢复展示状态/敏感材料；listener与已发worker由宿主独立收尾。
    // Cancel通常发生在原Call已撤销之后，不能再次申请context、联网或在Drop补发。
    Ok(Value::Null)
}
