//! 国内短信真实协议：agent-send-code / agent-login，验证码必须作为 JSON 数字。
//! 只认主机根 flow，deviceId/phone/票据按 flow 保存；不接受前端指定设备号。
//! 原生与公开参考未提供国内 Geetest 挑战形状，631002/630014 如实失败而非伪造挑战。
use agent2api_wasm_sdk::{self as sdk, json, login::sms as contract, state::{self, Scope, Snapshot}, Error, Result, Value};
use super::{account, client, util};

const KEY: &str = "sms-session";
const TTL: i64 = 300_000;

fn load(params: &Value) -> Result<Snapshot> {
    let flow = params["flow"].as_str().filter(|value| value.starts_with("sms_") && value.len() <= 256 && !value.contains('\0'));
    if flow.is_none() { return Err(Error::coded("主程序未提供可信的 AutoClaw 短信流程，请从当前账号面板重新发起", "login_flow_required")); }
    let snapshot = state::get(Scope::Login, KEY, 1)?;
    if !snapshot.compatible { return Err(Error::http(409, "AutoClaw 短信状态格式已变化，请重新发起")); }
    Ok(snapshot)
}
fn save(snapshot: &Snapshot, value: Value) -> Result<bool> { state::compare_exchange(Scope::Login, KEY, 1, snapshot.revision, value, TTL) }
fn parse(params: &Value) -> Result<contract::Request> {
    let request: contract::Request = serde_json::from_value(params["payload"].clone()).map_err(|_| Error::http(400, "AutoClaw 短信输入格式无效"))?;
    if request.method_index != 0 || request.country_code != "86" || !request.captcha.is_null() {
        return Err(Error::http(400, "AutoClaw 国内短信方式、区号或验证输入不符"));
    }
    Ok(request)
}
fn phone(raw: &str) -> Result<String> {
    let trimmed = raw.chars().filter(|ch| !ch.is_whitespace() && *ch != '-').collect::<String>();
    let phone = trimmed.strip_prefix("+86").unwrap_or(&trimmed);
    let phone = phone.strip_prefix("86").filter(|value| value.len() == 11).unwrap_or(phone);
    if phone.len() == 11 && phone.starts_with('1') && phone.as_bytes().get(1).is_some_and(|byte| (b'2'..=b'9').contains(byte))
        && phone.bytes().all(|byte| byte.is_ascii_digit()) { return Ok(phone.into()); }
    Err(Error::http(400, "请填写 11 位中国大陆手机号"))
}
fn code(raw: &str) -> Result<u32> {
    let raw = raw.trim();
    if raw.len() == 6 && raw.bytes().all(|byte| byte.is_ascii_digit()) {
        if let Ok(number) = raw.parse() { return Ok(number); }
    }
    Err(Error::http(400, "请填写 6 位数字验证码"))
}
fn success(payload: &Value) -> Result<()> {
    let code = util::integer(&payload["code"]);
    if code == Some(0) || payload.get("code").is_none() && payload["data"].is_object() { return Ok(()); }
    let message = match code {
        Some(400_001) => "请求参数有误，请检查手机号与验证码格式",
        Some(630_202) => "验证码不正确，请检查后重试",
        Some(630_101) => "获取验证码过于频繁，请稍后再试",
        Some(410_000 | 400_000 | 401) => "登录态已失效，请重新发起",
        Some(400_002) => "请求签名校验失败，请校准系统时间后重试",
        Some(631_002 | 630_014) => "上游要求额外的官方风控验证，当前短信协议没有可用验证步骤；请在官方客户端登录后导入凭据",
        _ => "AutoClaw 短信登录未完成，请稍后重试或改用凭据导入",
    };
    let mut error = Error::http(if code == Some(630_101) { 429 } else { 400 }, message);
    error.upstream_code = code;
    Err(error)
}

pub fn send(params: &Value) -> Result<Value> {
    let request = parse(params)?;
    let phone = phone(&request.phone)?;
    let before = load(params)?;
    let now = sdk::now();
    if matches!(before.value["phase"].as_str(), Some("cancelled" | "done")) { return Err(Error::http(409, "AutoClaw 短信流程已结束，请重新发起")); }
    if matches!(before.value["phase"].as_str(), Some("sending" | "verifying")) && before.value["busyUntil"].as_i64().unwrap_or(0) > now {
        return Err(Error::http(409, "AutoClaw 短信步骤正在处理，请稍后重试"));
    }
    if before.value["retryAt"].as_i64().unwrap_or(0) > now { return Err(Error::http(429, "验证码发送过于频繁，请等待倒计时结束")); }
    let device = util::random_hex(32)?;
    let marker = util::random_hex(16)?;
    let value = json!({ "phase": "sending", "marker": marker, "phone": phone, "deviceId": device,
        "retryAt": now + 60_000, "busyUntil": now + 60_000, "expiresAt": now + TTL });
    if !save(&before, value)? { return Err(Error::http(409, "AutoClaw 短信流程已变化")); }
    let reserved = load(params)?;
    if reserved.value["marker"] != marker || reserved.value["phase"] != "sending" { return Err(Error::http(409, "AutoClaw 短信流程已取消或被替换")); }
    let result = client::post("/userapi/v1/agent-send-code", "", &json!({ "phone": phone, "source_id": "autoclaw", "device_id": device }), "发送验证码")
        .and_then(|payload| { success(&payload)?;
            if payload["data"]["result"] != true { return Err(Error::http(502, "AutoClaw 未确认验证码已发送，请稍后重试")); }
            Ok(()) });
    if let Err(error) = result {
        let retry_at = if error.status == Some(429) { now + 60_000 } else { 0 };
        let _ = save(&reserved, json!({ "phase": "send_failed", "retryAt": retry_at }));
        return Err(error);
    }
    let ticket = util::random_hex(16)?;
    let value = json!({ "phase": "sent", "phone": phone, "deviceId": device, "ticket": ticket,
        "retryAt": now + 60_000, "expiresAt": sdk::now() + TTL, "attempts": 0 });
    if !save(&reserved, value)? { return Err(Error::http(409, "验证码已发送，但流程已取消或更新，请重新发起")); }
    Ok(contract::sent(json!({ "id": ticket }), 60, 300))
}

pub fn verify(params: &Value) -> Result<Value> {
    let request = parse(params)?;
    let phone = phone(&request.phone)?;
    let code = code(&request.code)?;
    let before = load(params)?;
    if before.value["phase"] != "sent" || before.value["expiresAt"].as_i64().unwrap_or(0) <= sdk::now() {
        return Err(Error::http(400, "AutoClaw 验证码未发送、已使用或过期，请重新获取"));
    }
    if before.value["phone"] != phone || before.value["ticket"] != request.ticket["id"] || !request.ticket["id"].is_string() {
        return Err(Error::http(400, "AutoClaw 手机号与验证码票据不属于当前流程"));
    }
    let attempts = before.value["attempts"].as_u64().unwrap_or(0);
    if attempts >= 5 { return Err(Error::http(429, "验证码尝试次数过多，请重新获取")); }
    let device = util::text(&before.value, &["deviceId"]);
    if device.len() != 64 || !device.bytes().all(|byte| byte.is_ascii_hexdigit()) { return Err(Error::http(400, "AutoClaw 短信流程缺少有效设备关联")); }
    let mut working = before.value.clone();
    working["phase"] = json!("verifying"); working["attempts"] = json!(attempts + 1); working["busyUntil"] = json!(sdk::now() + 60_000);
    if !save(&before, working)? { return Err(Error::http(409, "AutoClaw 短信流程已变化")); }
    let reserved = load(params)?;
    if reserved.value["phase"] != "verifying" || reserved.value["ticket"] != before.value["ticket"] { return Err(Error::http(409, "AutoClaw 短信流程已取消")); }
    let result = client::post("/userapi/v1/agent-login/", "", &json!({ "phone": phone, "code": code,
        "platform": "web", "source_id": "autoclaw", "device_id": device }), "验证码登录")
        .and_then(|payload| {
            success(&payload)?;
            let token = util::strip_bearer(&util::text(&payload["data"], &["access_token"]));
            let refresh = util::strip_bearer(&util::text(&payload["data"], &["refresh_token"]));
            if token.starts_with("enc:") || refresh.starts_with("enc:") {
                return Err(Error::http(502, "AutoClaw 短信接口返回了非预期凭据形态，未调用本机解密"));
            }
            let secret = account::parse(&json!({ "token": token, "refreshToken": refresh,
                "deviceId": device, "phoneTail": format!("{}****{}", &phone[..3], &phone[7..]) }))?;
            account::normalized(account::project(&secret))
        });
    match result {
        Ok(account) => {
            // 真实宿主在 verify 成功时立即清理 flow；不保存不可复取的 token 副本。
            if !save(&reserved, json!({ "phase": "done" }))? { return Err(Error::http(409, "AutoClaw 登录已取消，未保存凭据")); }
            Ok(account)
        }
        Err(error) => {
            let terminal = error.upstream_code != Some(630_202);
            let value = if terminal { json!({ "phase": "verify_failed", "retryAt": before.value["retryAt"] }) } else {
                let mut retry = before.value.clone(); retry["attempts"] = json!(attempts + 1); retry
            };
            let _ = save(&reserved, value);
            Err(error)
        }
    }
}

pub fn cancel() -> Result<Value> {
    for _ in 0..3 {
        let snapshot = state::get(Scope::Login, KEY, 1)?;
        if save(&snapshot, json!({ "phase": "cancelled" }))? { return Ok(Value::Null); }
    }
    Err(Error::http(409, "AutoClaw 短信状态仍在变化，请重试取消"))
}
