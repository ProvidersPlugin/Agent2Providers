//! CAccount 短信登录，两步签名 POST。msgid/手机号仅在可信根 flow 的登录状态内。
//! 新短信宿主必须把任务 flow 放在根 params.flow；不接受 payload.flow 或全局会话兜底。
//! 原生没有验证码挑战协议，不凭错误文案猜外部验证网址，也不扩大网络白名单。

use agent2api_wasm_sdk::{
    self as sdk, json,
    login::sms as contract,
    state::{self, Scope, Snapshot},
    Error, Result, Value,
};

use super::{account, client, errors, sign};

const KEY: &str = "sms-session";
const SCHEMA: u32 = 1;
const EXPIRES_MS: i64 = 300_000;
const RESEND_MS: i64 = 60_000;
const SEND_PATH: &str = "/login/phone/sendMsgCode";
const VERIFY_PATH: &str = "/login/phone/checkCode";

fn require_flow(params: &Value) -> Result<&str> {
    params["flow"]
        .as_str()
        .filter(|flow| !flow.is_empty() && flow.len() <= 256 && !flow.contains('\0'))
        .ok_or_else(|| {
            Error::coded(
                "主程序未提供可信的 Loomy 短信流程标识，请更新主程序后重新发起",
                "login_flow_required",
            )
        })
}

fn load(params: &Value) -> Result<Snapshot> {
    require_flow(params)?;
    let snapshot = state::get(Scope::Login, KEY, SCHEMA)?;
    if !snapshot.compatible {
        return Err(Error::new("Loomy 短信登录状态格式已变化，请重新发起"));
    }
    Ok(snapshot)
}

fn save(previous: &Snapshot, value: Value) -> Result<bool> {
    state::compare_exchange(
        Scope::Login,
        KEY,
        SCHEMA,
        previous.revision,
        value,
        EXPIRES_MS,
    )
}

fn parse(params: &Value) -> Result<contract::Request> {
    require_flow(params)?;
    let request: contract::Request = serde_json::from_value(params["payload"].clone())
        .map_err(|_| Error::http(400, "Loomy 短信输入格式无效"))?;
    if request.method_index != 0 || request.country_code != "86" {
        return Err(Error::http(400, "Loomy 短信登录方式或手机区号无效"));
    }
    Ok(request)
}

fn phone(raw: &str) -> Result<String> {
    let trimmed: String = raw
        .chars()
        .filter(|ch| !ch.is_whitespace() && *ch != '-')
        .collect();
    let digits = trimmed.strip_prefix("+86").unwrap_or(&trimmed);
    let digits = digits
        .strip_prefix("86")
        .filter(|rest| rest.len() == 11)
        .unwrap_or(digits);
    if digits.len() == 11
        && digits.starts_with('1')
        && digits
            .as_bytes()
            .get(1)
            .is_some_and(|digit| (b'3'..=b'9').contains(digit))
        && digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        Ok(digits.to_string())
    } else {
        Err(Error::http(400, "请填写 11 位中国大陆手机号"))
    }
}

fn code(raw: &str) -> Result<&str> {
    let code = raw.trim();
    if code.len() == 6 && code.bytes().all(|byte| byte.is_ascii_digit()) {
        Ok(code)
    } else {
        Err(Error::http(400, "请填写 6 位数字验证码"))
    }
}

fn base() -> Result<Value> {
    super::auth::base()
}

fn login_success(payload: &Value, operation: &str, secrets: &[&str]) -> Result<()> {
    let business = errors::business_code(payload);
    if business == errors::SUCCESS {
        return Ok(());
    }
    let mut error = errors::http_error(
        if errors::is_auth(&business) { 401 } else { 400 },
        payload,
        secrets,
    );
    error.message = format!("{operation}失败：{}", error.message);
    Err(error)
}

fn clear_msgid(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.remove("msgid");
    }
    value
}

pub fn send(params: &Value) -> Result<Value> {
    let request = parse(params)?;
    let phone = phone(&request.phone)?;
    let current = load(params)?;
    if current.value["phase"] == "cancelled" {
        return Err(Error::http(400, "Loomy 短信登录已取消，请重新发起"));
    }
    if matches!(
        current.value["phase"].as_str(),
        Some("sending" | "verifying")
    ) {
        return Err(Error::http(409, "当前短信步骤正在处理，请稍后重试"));
    }
    if current.value["phase"] == "done" {
        return Err(Error::http(409, "本次短信登录已完成，请勿重复发送"));
    }
    let now = sdk::now();
    if current.value["retryAt"].as_i64().unwrap_or(0) > now {
        return Err(Error::http(429, "验证码发送过于频繁，请稍后重试").with_code("sms_resend_wait"));
    }
    let marker = sign::random_hex(16)?;
    let reserved_value = json!({ "phase": "sending", "phone": phone, "marker": marker,
        "retryAt": now.saturating_add(RESEND_MS), "expiresAt": now.saturating_add(EXPIRES_MS) });
    if !save(&current, reserved_value)? {
        return Err(Error::http(409, "短信登录状态已变化，请重试"));
    }
    let reserved = load(params)?;
    if reserved.value["phase"] != "sending" || reserved.value["marker"] != marker {
        return Err(Error::http(409, "短信登录已取消或被其他操作更新"));
    }
    let body =
        json!({ "base": base()?, "param": { "ccode": "86", "phone": phone, "expire": 300 } });
    let response =
        client::signed_post(SEND_PATH, &body, "发送验证码", &[&phone]).and_then(|payload| {
            login_success(&payload, "发送验证码", &[&phone])?;
            let msgid = [
                "/data/msgid",
                "/data/msgId",
                "/data/data/msgid",
                "/data/data/msgId",
            ]
            .iter()
            .find_map(|path| {
                payload
                    .pointer(path)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
            })
            .ok_or_else(|| Error::http(502, "验证码已发出但上游未返回 msgid，请稍后重试"))?;
            if msgid.len() > 8192 {
                return Err(Error::http(502, "验证码会话标识超过限制"));
            }
            Ok(msgid.to_string())
        });
    let msgid = match response {
        Ok(msgid) => msgid,
        Err(error) => {
            let mut value = reserved.value.clone();
            value["phase"] = json!("send_failed");
            let _ = save(&reserved, clear_msgid(value));
            return Err(error);
        }
    };
    let ticket = sign::random_hex(16)?;
    let next = json!({ "phase": "sent", "phone": phone, "msgid": msgid, "ticket": ticket,
        "retryAt": reserved.value["retryAt"], "expiresAt": reserved.value["expiresAt"] });
    if !save(&reserved, next)? {
        return Err(Error::http(
            409,
            "验证码已发送，但本次流程已取消或更新，请重新发起",
        ));
    }
    // 返回不含手机号/msgid 的随机票据，只用于核对宿主的同一条短信状态。
    Ok(contract::sent(json!({ "id": ticket }), 60, 300))
}

pub fn verify(params: &Value) -> Result<Value> {
    let request = parse(params)?;
    let phone = phone(&request.phone)?;
    let code = code(&request.code)?;
    let current = load(params)?;
    if current.value["phase"] == "done" && current.value["ticket"] == request.ticket {
        return Ok(current.value["account"].clone());
    }
    if current.value["phase"] != "sent"
        || current.value["expiresAt"].as_i64().unwrap_or(0) <= sdk::now()
    {
        return Err(Error::http(
            400,
            "短信登录已取消、过期或未发送验证码，请重新获取",
        ));
    }
    if current.value["phone"] != phone
        || current.value["ticket"].as_str() != request.ticket["id"].as_str()
        || request.ticket["id"]
            .as_str()
            .is_none_or(|ticket| ticket.is_empty())
    {
        return Err(Error::http(400, "手机号或验证码票据不属于本次登录流程"));
    }
    let msgid = current.value["msgid"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::http(400, "本次短信登录缺少发码记录，请重新获取验证码"))?;
    let mut verifying = current.value.clone();
    verifying["phase"] = json!("verifying");
    if !save(&current, verifying)? {
        return Err(Error::http(409, "短信登录状态已变化，请重试"));
    }
    let reserved = load(params)?;
    if reserved.value["phase"] != "verifying" || reserved.value["ticket"] != current.value["ticket"]
    {
        return Err(Error::http(409, "短信登录已取消或被其他操作更新"));
    }
    let body = json!({ "base": base()?, "param": {
        "ccode": "86", "phone": phone, "mcode": code, "msgid": msgid, "expire": 14 * 24 * 3600
    } });
    let result = client::signed_post(VERIFY_PATH, &body, "验证码登录", &[&phone, code, msgid])
        .and_then(|payload| {
            login_success(&payload, "验证码登录", &[&phone, code, msgid])?;
            let data = super::auth::data(&payload)?;
            let session = account::token(data)?;
            let uid = account::identifier(data, &["userId", "userid", "user_id"]);
            let returned_phone = account::identifier(data, &["phone"]);
            if !returned_phone.is_empty() && self::phone(&returned_phone)? != phone {
                return Err(Error::http(
                    409,
                    "短信返回的账号不属于本次手机号，未添加账号",
                ));
            }
            account::normalized(&session, &uid, &phone, None)
        });
    match result {
        Ok(account) => {
            if !save(
                &reserved,
                json!({ "phase": "done", "ticket": request.ticket, "account": account }),
            )? {
                return Err(Error::http(409, "登录返回时流程已取消或更新，未添加账号"));
            }
            Ok(account)
        }
        Err(error) => {
            if matches!(error.status, Some(400 | 401 | 403 | 429)) {
                let _ = save(&reserved, current.value);
            } else {
                let mut value = clear_msgid(reserved.value.clone());
                value["phase"] = json!("verify_uncertain");
                let _ = save(&reserved, value);
            }
            Err(error)
        }
    }
}

pub fn start(params: &Value) -> Result<Value> {
    let flow = require_flow(params)?;
    let mut input = params.clone();
    let payload = input["payload"]
        .as_object_mut()
        .ok_or_else(|| Error::http(400, "短信登录参数格式无效"))?;
    payload.entry("methodIndex".to_string()).or_insert(json!(0));
    payload
        .entry("countryCode".to_string())
        .or_insert(json!("86"));
    send(&input)?;
    Ok(json!({ "flow": flow, "url": "", "poll_interval_ms": 0,
        "instructions": "请输入本次收到的 6 位数字验证码", "external_browser": false, "allowed_hosts": [] }))
}

pub fn callback(params: &Value) -> Result<Value> {
    let current = load(params)?;
    if current.value["phase"] == "done" {
        return Ok(sdk::login::done(current.value["account"].clone()));
    }
    let phone = current.value["phone"].as_str().unwrap_or("");
    let mut input = params.clone();
    input["payload"] = json!({ "methodIndex": 0, "countryCode": "86", "phone": phone,
        "code": params["code"], "ticket": { "id": current.value["ticket"] } });
    verify(&input).map(sdk::login::done)
}

pub fn poll(params: &Value) -> Result<Value> {
    let current = load(params)?;
    if current.value["phase"] == "done" {
        return Ok(sdk::login::done(current.value["account"].clone()));
    }
    if current.value["phase"] == "cancelled"
        || current.value["phase"] == "verify_uncertain"
        || current.value["expiresAt"].as_i64().unwrap_or(0) <= sdk::now()
    {
        return Ok(
            json!({ "state": "failed", "message": "Loomy 短信登录已取消或过期，请重新发起" }),
        );
    }
    Ok(json!({ "state": "pending", "hint": "等待本次短信验证码" }))
}

pub fn cancel(params: &Value) -> Result<Value> {
    for _ in 0..3 {
        let current = load(params)?;
        if save(&current, json!({ "phase": "cancelled" }))? {
            return Ok(Value::Null);
        }
    }
    Err(Error::new("短信登录状态正被更新，未覆盖其他操作"))
}
