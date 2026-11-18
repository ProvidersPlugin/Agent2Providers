//! 短信走宿主受控任务：首次只取极验配置，真人完成后才发短信，验证后同次换模型凭据。
//! ticket不含手机号/极验/凭据，仅本flow关联；不用payload.flow、全局会话或用户传device。

use super::{auth, captcha, client, endpoints, login_state as state};
use agent2api_wasm_sdk::{self as sdk, json, login::sms as contract, Error, Result, Value};

const RESEND_MS: i64 = 60_000;
const CODE_TTL: i64 = 300_000;
const CAPTCHA_TTL: i64 = 180_000;

fn input(params: &Value) -> Result<contract::Request> {
    state::flow(params, true)?;
    let request: contract::Request = serde_json::from_value(params["payload"].clone())
        .map_err(|_| Error::http(400, "AStudio 短信登录输入格式异常"))?;
    if request.method_index != 3 || request.country_code != "86" {
        return Err(Error::http(400, "AStudio 短信登录方式或区号不正确"));
    }
    if request.phone.len() != 11
        || !request.phone.starts_with('1')
        || !request.phone.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(Error::http(400, "请输入11位中国大陆手机号"));
    }
    Ok(request)
}

fn same_ticket(value: &Value, ticket: &Value) -> bool {
    ticket.as_object().is_some_and(|ticket| ticket.len() == 1)
        && value["ticket"]
            .as_str()
            .is_some_and(|id| !id.is_empty() && ticket["id"] == id)
}

pub fn send(params: &Value) -> Result<Value> {
    let request = input(params)?;
    if !request.code.is_empty() {
        return Err(Error::http(400, "发送短信时不能同时提交验证码"));
    }
    let previous = state::load()?;
    let now = sdk::now();
    if !previous.value.is_null() {
        state::live(&previous)?;
        if previous.value["phone"] != request.phone {
            return Err(Error::http(400, "请先在主程序中切换手机号再重新发送"));
        }
        if matches!(
            previous.value["phase"].as_str(),
            Some("fetching" | "sending" | "verifying")
        ) {
            return Err(Error::http(409, "AStudio 当前登录步骤正在处理"));
        }
        if previous.value["retryAt"]
            .as_i64()
            .is_some_and(|at| at > now)
        {
            return Err(
                Error::http(429, "请等待 AStudio 短信重发倒计时结束").with_code("sms_resend_wait")
            );
        }
    }
    let ready = previous.value["phase"] == "captcha"
        && previous.value["captchaUntil"]
            .as_i64()
            .is_some_and(|at| at > now);
    if !ready {
        let deadline = previous.value["deadline"]
            .as_i64()
            .unwrap_or_else(|| now.saturating_add(state::TTL));
        let reserved = state::reserve(
            &previous,
            json!({ "phone": request.phone, "deadline": deadline }),
            "fetching",
        )?;
        let config = match captcha::config() {
            Ok(config) => config,
            Err(error) => {
                let _ = state::save(
                    &reserved,
                    json!({ "phase": "send_failed", "phone": request.phone,
                    "deadline": deadline, "retryAt": now.saturating_add(RESEND_MS) }),
                );
                return Err(error);
            }
        };
        let ticket = state::nonce()?;
        state::save(
            &reserved,
            json!({ "phase": "captcha", "phone": request.phone, "deadline": deadline,
            "ticket": ticket, "config": config, "captchaUntil": now.saturating_add(CAPTCHA_TTL) }),
        )?;
        return Ok(contract::captcha(
            json!({ "id": ticket }),
            captcha::challenge(&config)?,
        ));
    }
    if !same_ticket(&previous.value, &request.ticket) {
        return Err(Error::http(400, "AStudio 短信挑战不属于当前登录流程"));
    }
    let mut fields = captcha::fields(&previous.value["config"], &request.captcha)?;
    fields["mobile"] = json!(request.phone);
    fields["countryCode"] = json!("86");
    let mut sending = previous.value.clone();
    sending["retryAt"] = json!(now.saturating_add(RESEND_MS));
    let reserved = state::reserve(&previous, sending, "sending")?;
    let sent = client::form(endpoints::SEND_SMS, fields).and_then(|reply| reply.form_ok());
    if let Err(error) = sent {
        // 一次性极验不重用，网络失败也不立即再次发码。
        let _ = state::save(
            &reserved,
            json!({ "phase": "send_failed", "phone": request.phone,
            "deadline": reserved.value["deadline"], "retryAt": now.saturating_add(RESEND_MS) }),
        );
        return Err(error);
    }
    let ticket = state::nonce()?;
    state::save(
        &reserved,
        json!({ "phase": "sent", "phone": request.phone, "ticket": ticket,
        "deadline": reserved.value["deadline"], "retryAt": now.saturating_add(RESEND_MS),
        "expiresAt": sdk::now().saturating_add(CODE_TTL), "attempts": 0 }),
    )?;
    Ok(contract::sent(json!({ "id": ticket }), 60, 300))
}

pub fn verify(params: &Value) -> Result<Value> {
    let request = input(params)?;
    let code = request.code.trim();
    if code.len() != 6
        || !code.bytes().all(|byte| byte.is_ascii_digit())
        || !request.captcha.is_null()
    {
        return Err(Error::http(400, "请输入本次收到的6位数字验证码"));
    }
    let previous = state::load()?;
    state::live(&previous)?;
    if previous.value["phase"] != "sent"
        || previous.value["phone"] != request.phone
        || !same_ticket(&previous.value, &request.ticket)
        || previous.value["expiresAt"]
            .as_i64()
            .is_none_or(|at| at <= sdk::now())
    {
        return Err(Error::http(
            400,
            "AStudio 短信未发送、已过期或与本次流程不符",
        ));
    }
    let attempts = previous.value["attempts"].as_u64().unwrap_or(0);
    if attempts >= 5 {
        return Err(Error::http(429, "验证码尝试次数过多，请重新获取"));
    }
    let mut running = previous.value.clone();
    running["attempts"] = json!(attempts + 1);
    let reserved = state::reserve(&previous, running, "verifying")?;
    let login = client::form(
        endpoints::LOGIN_SMS,
        json!({ "mobile": request.phone, "verifyCode": code, "countryCode": "86" }),
    )
    .and_then(|reply| reply.data(false, "session"));
    let data = match login {
        Ok(data) => data,
        Err(error) => {
            if matches!(
                error.code.as_deref(),
                Some("timeout" | "aborted" | "astudio_transport")
            ) {
                let _ = state::save(
                    &reserved,
                    json!({ "phase": "verify_unknown", "phone": request.phone,
                    "deadline": reserved.value["deadline"], "retryAt": sdk::now().saturating_add(RESEND_MS) }),
                );
            } else {
                let mut retry = previous.value.clone();
                retry["attempts"] = json!(attempts + 1);
                let _ = state::save(&reserved, retry);
            }
            return Err(error);
        }
    };
    // 验证码已兑出会话，不因后续接口失败再次兑换同一个码。
    let result = auth::complete(&data, "sms", || state::ensure_current(&reserved));
    state::save(&reserved, json!({ "phase": "done" }))?;
    result
}
