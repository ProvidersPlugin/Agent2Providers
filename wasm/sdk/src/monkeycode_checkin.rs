use crate::{host, http, json, Error, Result, Value};
use crate::state::{self, Scope};

fn origin(provider: &str) -> Result<&'static str> {
    match provider {
        "monkeycode" => Ok("https://monkeycode-ai.com"),
        "monkeycode-intl" => Ok("https://monkeycode-ai.net"),
        _ => Err(Error::http(403,"签到来源与提供商不符")),
    }
}

fn request(provider: &str, session: &str, method: &str, path: &str, body: Option<&Value>, envelope: bool) -> Result<Value> {
    let origin = origin(provider)?;
    let headers = json!({"Cookie":format!("monkeycode_ai_session={session}"),"Origin":origin,
        "Referer":format!("{origin}/console/"),"Accept":"application/json","Content-Type":"application/json"});
    let response = http::open_timeout(method, &format!("{origin}{path}"), headers, body, 30_000)?;
    let status = response.status;
    let bytes = response.bytes(1024 * 1024)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| Error::http(502,"上游未返回完整的签到或镜像资料"))?;
    if !(200..300).contains(&status) {
        return Err(Error::http(status, format!("MonkeyCode 接口返回 HTTP {status}"))
            .with_code("monkeycode_checkin_http_rejected"));
    }
    if envelope {
        if value["code"].as_i64() != Some(0) {
            let code = value["code"].as_i64();
            let mut error = Error::http(match code {
                Some(401 | 403 | 429) => code.unwrap_or(502) as u16,
                _ => 502,
            }, "MonkeyCode 未确认操作成功").with_code(if code.is_some() {
                "monkeycode_checkin_business_rejected"
            } else { "monkeycode_checkin_protocol_error" });
            error.upstream_code = code;
            return Err(error);
        }
        return value.get("data").filter(|data| data.is_object()).cloned()
            .ok_or_else(|| Error::http(502, "MonkeyCode 未返回完整操作状态")
                .with_code("monkeycode_checkin_protocol_error"));
    }
    Ok(value)
}

pub fn default_image(provider: &str, session: &str) -> Result<String> {
    let data = request(provider, session, "GET", "/api/v1/users/images?limit=100", None, true)?;
    let rows = data["images"].as_array().filter(|rows| !rows.is_empty() && rows.len() <= 100)
        .ok_or_else(|| Error::http(400,"账号没有可用的运行环境，请先在官网配置或在账号页填写镜像编号"))?;
    let defaults: Vec<_> = rows.iter().filter(|row| row["is_default"] == true).collect();
    let row = if defaults.len() == 1 { defaults[0] } else if rows.len() == 1 { &rows[0] }
        else { return Err(Error::http(400,"账号有多个运行环境且未指定默认项，请在账号页填写要使用的镜像编号")); };
    row["id"].as_str().filter(|id| uuid(id)).map(str::to_ascii_lowercase)
        .ok_or_else(|| Error::http(502,"官方运行环境没有返回有效编号"))
}

pub fn claim(provider: &str) -> Result<Value> {
    let _lock = crate::AccountLock::acquire()?;
    let account = crate::account()?;
    if account["provider"] != provider { return Err(Error::http(403,"签到账号归属已变化")); }
    let session = if provider == "monkeycode-intl" {
        let mut selected = None;
        for key in ["accessToken", "session", "token"] {
            let Some(raw) = account["secret"].get(key).filter(|value| !value.is_null()) else { continue; };
            let candidate = raw.as_str().ok_or_else(|| Error::http(401,"国际账号登录态字段无效，请重新登录"))?.trim();
            if candidate.is_empty() { continue; }
            if candidate.len() > 8192 || !candidate.bytes().all(|byte| {
                matches!(byte, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e)
            }) || selected.is_some_and(|previous| previous != candidate) {
                return Err(Error::http(401,"国际账号登录态无效或别名冲突，请重新登录"));
            }
            selected = Some(candidate);
        }
        selected.ok_or_else(|| Error::http(401,"国际账号没有有效登录态，请重新登录"))?
    } else {
        account["secret"]["accessToken"].as_str().filter(|session| !session.is_empty())
            .ok_or_else(|| Error::http(401,"账号没有有效登录态，请重新登录"))?
    };
    let status = request(provider, session, "GET", "/api/v1/users/wallet/checkin", None, true);
    unchanged(&account)?;
    let status = status?;
    if status["checked_in"] == true { return Ok(outcome("done","今天已签到",None)); }
    if status["checked_in"] != false { return Err(Error::http(502,"上游未返回明确的今日签到状态")); }
    let started = now()?;
    let day = started.div_euclid(86_400_000);
    let previous = state::get(Scope::Account,"monkeycode.checkin",1)?;
    if !previous.compatible && !previous.value.is_null() { return Err(Error::http(409,"签到记录版本不兼容，未覆盖旧记录")); }
    let recently_pending = previous.value["submittedAt"].as_i64().is_some_and(|at| {
        let elapsed = started.saturating_sub(at);
        (0..86_400_000).contains(&elapsed) || at > started
    });
    if previous.value["pending"] == true
        && (recently_pending || previous.value["day"].as_i64() == Some(day)) {
        return Ok(outcome("failed","上次签到结果仍未确认，本次只查询、不重复提交",None));
    }
    let challenge = request(provider, session,"POST","/api/v1/public/captcha/challenge",Some(&json!({})),false)?;
    let solved = host("crypto.monkeycode.cap.solve", json!({"challenge": challenge}))?;
    let solutions = solved["solutions"].as_array().filter(|rows| !rows.is_empty() && rows.len() <= 100
        && rows.len() as u64 == challenge["challenge"]["c"].as_u64().unwrap_or(0)
        && rows.iter().all(|row| row.as_u64().is_some())).ok_or_else(pow_error)?;
    let token = challenge["token"].as_str().filter(|token| !token.is_empty() && token.len() <= 256).ok_or_else(pow_error)?;
    let redeemed = request(provider, session,"POST","/api/v1/public/captcha/redeem",Some(&json!({"token":token,"solutions":solutions})),false)?;
    let token = redeemed["token"].as_str().filter(|token| redeemed["success"] == true && !token.is_empty() && token.len() <= 256).ok_or_else(pow_error)?;
    unchanged(&account)?;
    // 提交时刻仅约束未知结果的再次提交，不声明任一地区的官方日界。
    let submitted_at = now()?;
    if submitted_at.div_euclid(86_400_000) != day {
        return Ok(outcome("failed", "签到准备跨过日期边界，请重新查询今日状态", None));
    }
    let pending_value = json!({"day":day,"pending":true,"submittedAt":submitted_at});
    if !state::compare_exchange(Scope::Account,"monkeycode.checkin",1,previous.revision,pending_value.clone(),0)? {
        return Err(Error::http(409,"签到记录已变化，未重复提交"));
    }
    let pending = state::get(Scope::Account,"monkeycode.checkin",1)?;
    if !pending.compatible || pending.value != pending_value {
        return Err(Error::http(409, "签到提交记录已变化，未发送签到"));
    }
    unchanged(&account)?;
    let submitted = request(provider, session,"POST","/api/v1/users/wallet/checkin",Some(&json!({"captcha_token":token})),true);
    unchanged(&account)?;
    let after = request(provider,session,"GET","/api/v1/users/wallet/checkin",None,true);
    unchanged(&account)?;
    let after = after?;
    let current = state::get(Scope::Account,"monkeycode.checkin",1)?;
    if !current.compatible || current.revision != pending.revision || current.value != pending_value {
        return Err(Error::http(409, "签到结果对应的原提交记录已变化，未覆盖新记录"));
    }
    if after["checked_in"] != true {
        if let Err(error) = submitted {
            if after["checked_in"] == false && matches!(error.code.as_deref(),
                Some("monkeycode_checkin_http_rejected" | "monkeycode_checkin_business_rejected")) {
                if !state::compare_exchange(Scope::Account,"monkeycode.checkin",1,pending.revision,
                    json!({"day":day,"pending":false,"rejected":true}),0)? {
                    return Err(Error::http(409, "明确拒绝后的原签到记录已变化，未覆盖"));
                }
            }
            return Err(error);
        }
        return Ok(outcome("failed","签到已提交，官方尚未确认今日完成",None));
    }
    if !state::compare_exchange(Scope::Account,"monkeycode.checkin",1,pending.revision,json!({"day":day,"pending":false,"confirmed":true}),0)? {
        return Err(Error::http(409,"签到已被官方确认，本地记录保存未确认"));
    }
    unchanged(&account)?;
    Ok(outcome("claimed","签到已由官方确认",None))
}

fn outcome(kind: &str,message: &str,credit: Option<f64>) -> Value {
    json!({"outcome":kind,"message":message,"credit":credit,"streak":null})
}
fn now() -> Result<i64> { host("clock.now",Value::Null)?.as_i64().filter(|now| *now > 0).ok_or_else(pow_error) }
fn unchanged(account: &Value) -> Result<()> {
    let next = crate::account()?;
    if next["id"] != account["id"] || next["provider"] != account["provider"] || next["secret"] != account["secret"] {
        return Err(Error::http(409,"签到账号发生变化，未继续操作"));
    }
    Ok(())
}
fn pow_error() -> Error { Error::http(503,"官方计算验证资料无效、过期或超过计算预算，请稍后重试") }
fn uuid(value: &str) -> bool {
    value.len() == 36 && value.bytes().enumerate().all(|(index,byte)| if matches!(index,8|13|18|23) { byte == b'-' } else { byte.is_ascii_hexdigit() })
        && value.bytes().any(|byte| matches!(byte,b'1'..=b'9'|b'a'..=b'f'|b'A'..=b'F'))
}
