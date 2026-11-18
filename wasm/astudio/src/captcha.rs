//! 极验v3字段逐字对应原生auth.rs。上游只将seccode做Base64，challenge/validate原样发送。
//! 仅验证用户交回的三件与本次挑战绑定；不自动解题、不接验证码绕过服务。

use super::{client, endpoints};
use agent2api_wasm_sdk::{
    json,
    login::{sms, Geetest},
    Error, Result, Value,
};
use base64::{engine::general_purpose::STANDARD, Engine};

pub fn config() -> Result<Value> {
    let mut reply = client::studio("GET", endpoints::CAPTCHA, None)?;
    // 裸极验配置的success=false表示离线挑战，不是API失败信封。
    let bare = reply.body["gt"].is_string() && reply.body["challenge"].is_string();
    let success = reply.body["success"].clone();
    if bare {
        if let Some(object) = reply.body.as_object_mut() {
            object.remove("success");
        }
    }
    let mut data = reply.data(false, "session")?;
    if bare {
        data.as_object_mut()
            .ok_or_else(|| Error::http(502, "AStudio 极验配置格式不正确"))?
            .insert("success".into(), success);
    }
    let gt = data["gt"].as_str().unwrap_or("");
    let challenge = data["challenge"].as_str().unwrap_or("");
    if gt.is_empty()
        || gt.len() > 128
        || challenge.is_empty()
        || challenge.len() > 256
        || gt.chars().any(char::is_control)
        || challenge.chars().any(char::is_control)
    {
        return Err(Error::http(502, "AStudio 极验配置格式不正确"));
    }
    let success = data["success"]
        .as_bool()
        .or_else(|| data["success"].as_i64().map(|value| value != 0))
        .ok_or_else(|| Error::http(502, "AStudio 极验配置缺少success"))?;
    let new_captcha = data["new_captcha"]
        .as_bool()
        .ok_or_else(|| Error::http(502, "AStudio 极验配置缺少new_captcha"))?;
    Ok(json!({ "gt": gt, "challenge": challenge, "success": success, "new_captcha": new_captcha }))
}

pub fn interaction(config: &Value) -> Result<Geetest> {
    serde_json::from_value(config.clone()).map_err(|_| Error::http(502, "AStudio 极验状态无效"))
}

pub fn challenge(config: &Value) -> Result<sms::Challenge> {
    let value = interaction(config)?;
    Ok(sms::Challenge::Geetest {
        gt: value.gt,
        challenge: value.challenge,
        success: value.success,
        new_captcha: value.new_captcha,
    })
}

pub fn fields(config: &Value, input: &Value) -> Result<Value> {
    let invalid = || Error::http(400, "请完成当前 AStudio 登录步骤的滑块验证");
    let source = config["challenge"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(invalid)?;
    if !input.as_object().is_some_and(|object| object.len() == 3) {
        return Err(invalid());
    }
    let challenge = input["geetest_challenge"]
        .as_str()
        .filter(|value| !value.is_empty() && value.len() <= 512 && value.starts_with(source))
        .ok_or_else(invalid)?;
    let validate = input["geetest_validate"]
        .as_str()
        .filter(|value| !value.is_empty() && value.len() <= 1024)
        .ok_or_else(invalid)?;
    let seccode = input["geetest_seccode"]
        .as_str()
        .filter(|value| !value.is_empty() && value.len() <= 1024)
        .ok_or_else(invalid)?;
    if [challenge, validate, seccode]
        .iter()
        .any(|value| value.chars().any(char::is_control))
    {
        return Err(invalid());
    }
    Ok(
        json!({ "geetest_challenge": challenge, "geetest_validate": validate,
        "geetest_seccode": STANDARD.encode(seccode.as_bytes()) }),
    )
}
