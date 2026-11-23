use agent2api_wasm_sdk::{
    self as sdk, json,
    login::sms as contract,
    state::{self, Scope, Snapshot},
    Error, Result, Value,
};

use crate::{account, endpoints, login_phone as phone_flow, network, util};

const KEY: &str = "codar-sms";
const TTL: i64 = 600_000;

fn load_state(params: &Value) -> Result<Snapshot> {
    let flow = params["flow"]
        .as_str()
        .filter(|value| value.starts_with("sms_") && value.len() <= 256 && !value.contains('\0'));
    if flow.is_none() {
        return Err(Error::coded(
            "主程序未提供可信的科搭短信流程，请重新发起",
            "login_flow_required",
        ));
    }
    let snapshot = state::get(Scope::Login, KEY, 1)?;
    if !snapshot.compatible {
        return Err(Error::http(409, "科搭短信状态格式已变化，请重新发起"));
    }
    Ok(snapshot)
}

fn save_state(snapshot: &Snapshot, value: Value) -> Result<()> {
    if !state::compare_exchange(Scope::Login, KEY, 1, snapshot.revision, value, TTL)? {
        return Err(Error::http(409, "科搭短信步骤已变化，请重试"));
    }
    Ok(())
}

fn parse_request(params: &Value) -> Result<contract::Request> {
    let request: contract::Request = serde_json::from_value(params["payload"].clone())
        .map_err(|_| Error::http(400, "科搭短信输入格式无效"))?;
    if request.method_index != 1 || request.country_code != "86" {
        return Err(Error::http(400, "科搭短信登录方式或区号无效"));
    }
    Ok(request)
}

fn check_marker(expected: &str) -> Result<Snapshot> {
    let current = state::get(Scope::Login, KEY, 1)?;
    if !current.compatible {
        return Err(Error::http(409, "科搭短信状态格式已变化，请重新发起"));
    }
    if current.value["marker"].as_str() != Some(expected) {
        return Err(Error::http(409, "科搭短信步骤已取消或发生变化，未保存结果"));
    }
    Ok(current)
}

pub fn send(params: &Value) -> Result<Value> {
    let request = parse_request(params)?;
    let phone = phone_flow::phone(&request.phone)?;
    let mut body = phone_flow::captcha(&request.captcha)?;
    let submitted = body.to_string();
    let fingerprint = util::digest(&submitted);
    let before = load_state(params)?;
    let now = sdk::now();
    if matches!(before.value["phase"].as_str(), Some("cancelled" | "done")) {
        return Err(Error::http(409, "科搭短信流程已结束，请重新发起"));
    }
    if matches!(before.value["phase"].as_str(), Some("sending" | "verifying"))
        && before.value["busyUntil"].as_i64().unwrap_or(0) > now
    {
        return Err(Error::http(409, "科搭短信步骤正在处理，请稍后重试"));
    }
    if before.value["resendAt"].as_i64().unwrap_or(0) > now {
        return Err(Error::http(429, "短信发送过于频繁，请等待倒计时结束"));
    }
    if before.value["captchaFingerprint"].as_str() == Some(fingerprint.as_str()) {
        return Err(Error::http(409, "此人机验证结果已经提交，请重新完成验证"));
    }
    let purpose = if before.value["purpose"].as_str() == Some("ip_verify") {
        if before.value["phone"].as_str() != Some(phone.as_str()) {
            return Err(Error::http(409, "二次验证手机号与原输入不一致，请点击更换后重来"));
        }
        let tail = util::text(&before.value, &["phoneTail"]);
        if !tail.is_empty() && !phone.ends_with(&tail) {
            return Err(Error::http(400, "手机号与科搭账号绑定尾号不一致"));
        }
        "ip_verify"
    } else {
        "login"
    };

    body["phone"] = json!(phone);
    body["name"] = json!("user");
    body["type"] = json!(purpose);

    let marker = util::nonce()?;
    let mut reserved_val = before.value.clone();
    if !reserved_val.is_object() {
        reserved_val = json!({});
    }
    reserved_val["phase"] = json!("sending");
    reserved_val["marker"] = json!(marker);
    reserved_val["busyUntil"] = json!(now.saturating_add(15_000));
    reserved_val["purpose"] = json!(purpose);
    reserved_val["phone"] = json!(phone);
    save_state(&before, reserved_val)?;

    let reply = network::request(
        "POST",
        endpoints::ACCOUNT,
        endpoints::SMS,
        None,
        Some(&body),
        12_000,
    )
    .and_then(|reply| reply.checked(&[phone.clone()]));
    let current = check_marker(&marker)?;
    let finished_at = sdk::now();
    let mut next = current.value.clone();
    if let Some(fields) = next.as_object_mut() {
        fields.remove("busyUntil");
        fields.remove("marker");
    }
    next["captchaFingerprint"] = json!(fingerprint);

    match reply {
        Ok(_) => {
            next["phase"] = json!("code");
            next["phone"] = json!(phone);
            next["purpose"] = json!(purpose);
            next["codeUntil"] = json!(finished_at.saturating_add(300_000));
            next["resendAt"] = json!(finished_at.saturating_add(60_000));
            save_state(&current, next)?;
            Ok(contract::sent(
                json!({ "purpose": purpose, "phone": phone }),
                60,
                300,
            ))
        }
        Err(error) => {
            next["phase"] = json!(if purpose == "ip_verify" {
                "ip_verify"
            } else {
                "phone"
            });
            if error.status == Some(429) {
                next["resendAt"] = json!(finished_at.saturating_add(60_000));
            }
            save_state(&current, next)?;
            let temp = util::text(&current.value, &["tempToken"]);
            let safe = util::safe_text(&error.message, &[temp], 400);
            Err(Error::http(error.status.unwrap_or(502), safe))
        }
    }
}

pub fn verify(params: &Value) -> Result<Value> {
    let request = parse_request(params)?;
    let phone = phone_flow::phone(&request.phone)?;
    let code = request.code.trim();
    if !(4..=8).contains(&code.len()) || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Error::http(400, "请填写 4–8 位数字短信验证码"));
    }
    let before = load_state(params)?;
    let now = sdk::now();
    if matches!(before.value["phase"].as_str(), Some("cancelled" | "done")) {
        return Err(Error::http(409, "科搭短信流程已结束，请重新发起"));
    }
    if matches!(before.value["phase"].as_str(), Some("sending" | "verifying"))
        && before.value["busyUntil"].as_i64().unwrap_or(0) > now
    {
        return Err(Error::http(409, "科搭短信步骤正在处理，请稍后重试"));
    }
    if before.value["phase"].as_str() != Some("code")
        || before.value["codeUntil"].as_i64().unwrap_or(0) <= now
    {
        return Err(Error::http(409, "短信验证码已过期，请重新获取"));
    }
    if before.value["phone"].as_str() != Some(phone.as_str()) {
        return Err(Error::http(409, "手机号已变化，请重新获取验证码"));
    }
    let purpose = before.value["purpose"].as_str().unwrap_or("login").to_string();
    let temp = before.value["tempToken"].as_str().unwrap_or("").to_string();
    let username = before.value["username"].as_str().unwrap_or("").to_string();
    let (path, token, body) = match purpose.as_str() {
        "login" => (
            endpoints::PHONE,
            None,
            json!({ "username": phone, "password": code }),
        ),
        "ip_verify" => (
            endpoints::VERIFY_IP,
            Some(temp.as_str()),
            json!({ "username": username, "phone": phone, "code": code }),
        ),
        _ => return Err(Error::http(409, "科搭验证用途无效")),
    };

    let marker = util::nonce()?;
    let mut reserved_val = before.value.clone();
    reserved_val["phase"] = json!("verifying");
    reserved_val["marker"] = json!(marker);
    reserved_val["busyUntil"] = json!(now.saturating_add(15_000));
    save_state(&before, reserved_val)?;

    let reply = network::request("POST", endpoints::ACCOUNT, path, token, Some(&body), 12_000);
    let current = check_marker(&marker)?;
    let mut restored = current.value.clone();
    if let Some(fields) = restored.as_object_mut() {
        fields.remove("busyUntil");
        fields.remove("marker");
    }

    let reply = match reply {
        Ok(reply) => reply,
        Err(error) => {
            restored["phase"] = json!("code");
            save_state(&current, restored)?;
            let safe = util::safe_text(&error.message, &[temp], 400);
            return Err(Error::http(
                error.status.unwrap_or(502),
                format!("本次验证结果未确认，请重试：{safe}"),
            ));
        }
    };

    if purpose == "login" {
        if let Some(challenge) = phone_flow::parse_phone_challenge(&reply.body)? {
            if !challenge.phone_tail.is_empty() && !phone.ends_with(&challenge.phone_tail) {
                restored["phase"] = json!("code");
                save_state(&current, restored)?;
                return Err(Error::http(
                    409,
                    "科搭短信登录账号与原手机号不一致，未继续验证",
                ));
            }
            if let Some(fields) = restored.as_object_mut() {
                for key in ["codeUntil", "resendAt", "captchaFingerprint"] {
                    fields.remove(key);
                }
            }
            restored["phase"] = json!("ip_verify");
            restored["purpose"] = json!("ip_verify");
            restored["phone"] = json!(phone);
            restored["tempToken"] = json!(challenge.temp_token);
            restored["username"] = json!(challenge.username);
            restored["phoneTail"] = json!(challenge.phone_tail);
            save_state(&current, restored)?;
            return Ok(contract::verify_captcha(
                json!({ "purpose": "ip_verify", "phone": phone }),
                contract::Challenge::Alicom4 {
                    captcha_id: endpoints::CAPTCHA_ID.into(),
                },
                "检测到非常用网络环境，请完成二次人机验证并获取验证码",
            ));
        }
    }

    let reply = match reply.checked(&[phone.clone(), code.to_string(), temp.clone()]) {
        Ok(reply) => reply,
        Err(error) => {
            restored["phase"] = json!("code");
            save_state(&current, restored)?;
            let safe = util::safe_text(&error.message, &[temp], 400);
            return Err(Error::http(error.status.unwrap_or(400), safe));
        }
    };

    let raw = util::text(network::data(&reply), &["token"]);
    let raw = if raw.is_empty() {
        util::text(&reply, &["token"])
    } else {
        raw
    };
    let normalized = account::from_token(&raw, None, "sms")?;
    if purpose == "ip_verify"
        && !username.is_empty()
        && normalized["secret"]["username"].as_str() != Some(username.as_str())
    {
        restored["phase"] = json!("code");
        save_state(&current, restored)?;
        return Err(Error::http(409, "科搭验证结果与原账号不一致，未保存"));
    }

    let mut done = json!({ "phase": "done" });
    if let Some(fields) = restored.as_object() {
        if let Some(fp) = fields.get("captchaFingerprint") {
            done["captchaFingerprint"] = fp.clone();
        }
    }
    save_state(&current, done)?;
    Ok(normalized)
}
