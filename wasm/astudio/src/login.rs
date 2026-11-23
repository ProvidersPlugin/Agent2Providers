//! 原生methodIndex=1继续为交互式登录；WASM用主程序表单+极验替代私有回环网页。
//! 密码只存在当前调用栈和multipart正文，不进入flow状态或公开结果。

use super::{auth, captcha, client, endpoints, login_state as state};
use agent2api_wasm_sdk::{
    self as sdk, json,
    login::{Field, Interaction},
    Error, Result, Value,
};

fn prompt(key: &str, label: &str, secret: bool) -> Field {
    Field {
        key: key.into(),
        label: label.into(),
        kind: "text".into(),
        secret,
        optional: false,
        options: Vec::new(),
    }
}

fn interaction(value: &Value) -> Result<Interaction> {
    let text = super::i18n::password_text(value["locale"].as_str().unwrap_or("zh-CN"));
    Ok(Interaction {
        id: value["step"].as_str().unwrap_or("").to_string(),
        title: text[0].into(),
        message: value["message"].as_str().unwrap_or(text[1]).to_string(),
        fields: vec![
            prompt("accountName", text[2], false),
            prompt("accountPwd", text[3], true),
        ],
        captcha: Some(captcha::interaction(&value["config"])?),
    })
}

pub fn start(params: &Value) -> Result<Value> {
    let flow = state::flow(params, false)?;
    let requested = params["payload"]["methodIndex"].as_u64();
    if params["method"]
        .as_str()
        .is_some_and(|method| method != "browser")
        || requested.is_some_and(|index| index != 1)
        || params["payload"].get("methodIndex").is_some() && requested.is_none()
    {
        return Err(Error::http(
            400,
            "请选择 AStudio 账号密码登录；短信请使用短信登录入口",
        ));
    }
    let previous = state::load()?;
    if !previous.value.is_null() {
        return Err(Error::http(409, "AStudio 登录流程已存在，请重新发起"));
    }
    let config = captcha::config()?;
    let locale = params["payload"]["locale"]
        .as_str()
        .filter(|value| matches!(*value, "en" | "ja" | "ko" | "pt-BR" | "zh-Hant"))
        .unwrap_or("zh-CN");
    let value = json!({ "phase": "password", "step": state::nonce()?, "config": config, "locale": locale,
        "deadline": sdk::now().saturating_add(state::TTL), "captchaUntil": sdk::now().saturating_add(180_000), "attempts": 0 });
    state::save(&previous, value.clone())?;
    let mut started = sdk::login::start(flow, interaction(&value)?);
    started["login_ui"] = json!({ "browserMode": "none", "manualCallback": false,
        "callbackInput": "url", "cookieCapture": null, "captureCallback": false });
    Ok(started)
}

fn retry(previous: &sdk::state::Snapshot, attempts: u64, message: &str) -> Result<Value> {
    state::ensure_current(previous)?;
    if attempts >= 5 {
        state::save(previous, json!({ "phase": "done" }))?;
        return Ok(json!({ "state": "failed", "message": "AStudio 登录尝试过多，请重新发起" }));
    }
    let config = match captcha::config() {
        Ok(config) => config,
        Err(error) => {
            state::save(previous, json!({ "phase": "done" }))?;
            return Ok(json!({ "state": "failed", "message": error.message }));
        }
    };
    let next = json!({ "phase": "password", "step": state::nonce()?, "config": config, "locale": previous.value["locale"],
        "deadline": previous.value["deadline"], "captchaUntil": sdk::now().saturating_add(180_000),
        "attempts": attempts, "message": message });
    state::save(previous, next.clone())?;
    Ok(sdk::login::pending(Some(interaction(&next)?)))
}

pub fn submit(params: &Value) -> Result<Value> {
    state::flow(params, false)?;
    let previous = state::load()?;
    state::live(&previous)?;
    if previous.value["phase"] != "password" || params["interaction"] != previous.value["step"] {
        return Err(Error::http(409, "AStudio 密码登录步骤已变化，请刷新表单"));
    }
    let attempts = previous.value["attempts"].as_u64().unwrap_or(0);
    let account = params["inputs"]["accountName"]
        .as_str()
        .unwrap_or("")
        .trim();
    let password = params["inputs"]["accountPwd"].as_str().unwrap_or("");
    if account.is_empty()
        || account.len() > 256
        || account.chars().any(char::is_control)
        || !(6..=20).contains(&password.len())
    {
        return Err(Error::http(400, "请填写有效账号及6–20位密码"));
    }
    if previous.value["captchaUntil"]
        .as_i64()
        .is_none_or(|at| at <= sdk::now())
    {
        return retry(&previous, attempts, "极验已过期，请重新完成滑块验证");
    }
    let mut fields = captcha::fields(&previous.value["config"], &params["captcha"])?;
    fields["accountName"] = json!(account);
    fields["accountPwd"] = json!(password);
    let reserved = state::reserve(&previous, previous.value.clone(), "verifying")?;
    let data = match client::form(endpoints::LOGIN_PASSWORD, fields)
        .and_then(|reply| reply.data(false, "session"))
    {
        Ok(data) => data,
        Err(error)
            if matches!(
                error.code.as_deref(),
                Some("timeout" | "aborted" | "astudio_transport")
            ) =>
        {
            state::save(&reserved, json!({ "phase": "done" }))?;
            return Ok(
                json!({ "state": "failed", "message": "AStudio 密码登录结果未能确认，请重新发起；不会重放已提交的滑块" }),
            );
        }
        Err(error) => return retry(&reserved, attempts + 1, &error.message),
    };
    let account = match auth::complete(&data, "password", || state::ensure_current(&reserved)) {
        Ok(account) => account,
        Err(error) => {
            state::save(&reserved, json!({ "phase": "done" }))?;
            return Ok(json!({ "state": "failed", "message": error.message }));
        }
    };
    state::save(&reserved, json!({ "phase": "done" }))?;
    Ok(sdk::login::done(account))
}

pub fn poll(params: &Value) -> Result<Value> {
    state::flow(params, false)?;
    let value = state::load()?;
    if state::live(&value).is_err() {
        return Ok(json!({ "state": "failed", "message": "AStudio 登录已结束或过期，请重新发起" }));
    }
    if value.value["phase"] == "password" {
        Ok(sdk::login::pending(Some(interaction(&value.value)?)))
    } else {
        Ok(json!({ "state": "pending", "hint": "正在完成 AStudio 登录" }))
    }
}
