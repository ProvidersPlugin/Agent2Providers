//! 国际版领取验证只用本账号状态与宿主租约，不共享国内或模型验证码票据。
use agent2api_wasm_sdk::{self as sdk, json, verification as lease, Error, Result, Value};
use super::{account, captcha, config, network::{self, PROVIDER}};

const CONVERSATION: &str = "zcode-intl-configuration";
const KEY: &str = "claim-verification";
const CLIENT_TTL: i64 = 40_000;
const VERIFY_TTL: i64 = 50_000;
const REPLAY_TTL: i64 = 120_000;

fn changed() -> Error { Error::http(409, "国际版领取账号或配置已变化，请重新获取验证码配置") }
fn text<'a>(value: &'a Value, key: &str) -> &'a str { value[key].as_str().unwrap_or("") }

fn bound(payload: &Value, record: &Value) -> Result<()> {
    if payload["provider"] != PROVIDER || payload["accountId"] != record["id"] {
        return Err(Error::http(400, "领取验证必须明确绑定当前国际版账号与提供商"));
    }
    Ok(())
}

fn proof(value: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 16_384 || value.contains(['\r', '\n', '\0']) {
        return Err(Error::http(428, "请为当前国际版账号重新完成领取验证"));
    }
    Ok(())
}

fn consume(state: &mut Value, param: &str, now: i64) -> Result<()> {
    let hash = network::hash(param);
    let used = state["consumed"].as_array_mut().ok_or_else(|| Error::new("领取验证状态异常"))?;
    if used.iter().any(|item| item["hash"] == hash) { return Err(Error::http(428, "这份领取验证码已经使用，请重新验证")); }
    if used.len() >= 128 { return Err(Error::http(429, "近期领取验证过多，请稍后重试")); }
    used.push(json!({ "hash": hash, "at": now }));
    Ok(())
}

fn change<T>(mut action: impl FnMut(&mut Value, i64) -> Result<T>) -> Result<T> {
    let record = account::record()?;
    let binding = config::model_verification_binding(&record);
    for _ in 0..8 {
        let snapshot = config::transient(KEY)?;
        let mut value = snapshot.value.clone();
        if !snapshot.compatible || !value.is_object() || value["binding"] != binding {
            value = json!({ "binding": binding, "clients": {}, "request": null, "consumed": [] });
        }
        if !value["clients"].is_object() { value["clients"] = json!({}); }
        if !value["consumed"].is_array() { value["consumed"] = json!([]); }
        let now = sdk::now();
        if let Some(clients) = value["clients"].as_object_mut() {
            clients.retain(|_, client| client["seenAt"].as_i64().is_some_and(|at| at > 0 && at <= now && now.saturating_sub(at) < CLIENT_TTL));
        }
        if let Some(used) = value["consumed"].as_array_mut() {
            used.retain(|item| item["at"].as_i64().is_some_and(|at| at > 0 && at <= now && now.saturating_sub(at) < REPLAY_TTL));
        }
        if value["request"].is_object() {
            let id = text(&value["request"], "requestId");
            if id.is_empty() || !lease::check(id)?.valid { value["request"] = Value::Null; }
        }
        let result = action(&mut value, now)?;
        if config::model_verification_binding(&account::record()?) != binding { return Err(changed()); }
        if config::put_transient(KEY, &snapshot, value, REPLAY_TTL)? { return Ok(result); }
    }
    Err(Error::http(409, "国际版领取验证状态正在变化，请重试"))
}

pub fn prepare(plan_id: &str, name: &str, deadline: i64) -> Result<Option<(String, String)>> {
    let record = account::record()?;
    let binding = config::model_verification_binding(&record);
    let configuration = captcha::configuration(true)?;
    if config::model_verification_binding(&account::record()?) != binding { return Err(changed()); }
    if configuration["enabled"] == false { return Ok(None); }
    let budget = deadline.saturating_sub(sdk::now()).saturating_sub(35_000).min(VERIFY_TTL);
    if budget < 5_000 { return Err(Error::http(428, "本轮剩余时间不足以完成验证，请在套餐明细重新领取")); }
    change(|state, _| {
        if state["request"].is_object() { return Err(Error::http(428, "当前国际版账号已有领取等待验证")); }
        if !state["clients"].as_object().into_iter().flat_map(|clients| clients.values()).any(|client| client["ready"] == true) {
            return Err(Error::http(428, "没有可用的账号验证界面，请打开管理面板后重试"));
        }
        Ok(())
    })?;
    let mut wait = lease::open(plan_id, text(&configuration, "configId"), budget as u64,
        lease::StateSlot { conversation: CONVERSATION, key: KEY, schema: 1 })?;
    let request_id = wait.request_id().to_string();
    change(|state, _| {
        if state["request"].is_object() { return Err(Error::http(428, "当前国际版账号已有领取等待验证")); }
        state["request"] = json!({ "requestId": request_id, "provider": PROVIDER, "accountId": record["id"],
            "planId": plan_id, "planName": name, "configId": configuration["configId"], "config": configuration,
            "expiresAt": wait.expires_at(), "clientId": null, "reply": null });
        Ok(())
    })?;
    loop {
        if config::model_verification_binding(&account::record()?) != binding { return Err(changed()); }
        let reply = change(|state, _| {
            let request = &state["request"];
            if request["requestId"] != request_id { return Err(Error::http(428, "领取验证已取消或过期")); }
            if !request["reply"].is_object() { return Ok(None); }
            let reply = request["reply"].clone();
            state["request"] = Value::Null;
            Ok(Some(reply))
        })?;
        if let Some(reply) = reply {
            wait.close()?;
            if reply["status"] != "verified" { return Err(Error::http(428, "领取验证未通过或已取消")); }
            let param = text(&reply, "param").to_string();
            proof(&param)?;
            return Ok(Some((param, text(&configuration, "region").to_string())));
        }
        sdk::sleep(500)?;
    }
}

pub fn check_supplied(payload: &Value, record: &Value) -> Result<Option<(String, String)>> {
    bound(payload, record)?;
    let plan = text(payload, "planId");
    if plan.trim().is_empty() || plan.len() > 256 || plan.chars().any(char::is_control) { return Err(Error::http(400, "领取必须明确套餐")); }
    let configuration = captcha::cached_configuration(record)?;
    if payload["configId"] != configuration["configId"] || payload["captchaRegion"] != configuration["region"] { return Err(changed()); }
    let param = payload["captchaVerifyParam"].as_str().ok_or_else(|| Error::http(400, "领取缺少验证码字段"))?;
    if configuration["enabled"] == false {
        if !param.is_empty() { return Err(Error::http(400, "免验证配置不接受旧验证码")); }
        return Ok(None);
    }
    proof(param)?;
    Ok(Some((param.to_string(), text(&configuration, "region").to_string())))
}

pub fn supplied(payload: &Value, record: &Value) -> Result<Option<(String, String)>> {
    let result = check_supplied(payload, record)?;
    if let Some((param, _)) = &result {
        change(|state, now| {
            if state["request"].is_object() { return Err(Error::http(428, "当前国际版账号已有领取等待验证")); }
            consume(state, param, now)
        })?;
    }
    Ok(result)
}

pub fn poll(payload: &Value) -> Result<Value> {
    let record = account::record()?;
    bound(payload, &record)?;
    let client = text(payload, "clientId");
    if client.trim().is_empty() || client.len() > 128 || client.chars().any(char::is_control) { return Err(Error::http(400, "验证界面标识无效")); }
    change(|state, now| {
        let clients = state["clients"].as_object_mut().ok_or_else(|| Error::new("领取验证状态异常"))?;
        if !clients.contains_key(client) && clients.len() >= 32 { return Err(Error::http(429, "验证界面过多，请关闭多余面板")); }
        clients.insert(client.to_string(), json!({ "ready": payload["ready"] == true, "seenAt": now }));
        let request = &mut state["request"];
        let owned = request.is_object() && request["clientId"] == client;
        let select = owned || request.is_object() && request["clientId"].is_null() && payload["ready"] == true;
        let task = if select && request["accountId"] == record["id"] && request["reply"].is_null() {
            request["clientId"] = json!(client);
            json!({ "requestId": request["requestId"], "provider": PROVIDER, "accountId": record["id"],
                "planId": request["planId"], "planName": request["planName"], "expiresAt": request["expiresAt"],
                "remainingMs": request["expiresAt"].as_i64().unwrap_or(0).saturating_sub(now), "config": request["config"] })
        } else { Value::Null };
        Ok(json!({ "supported": true, "pollMs": 5000, "active": !task.is_null(), "task": task }))
    })
}

pub fn complete(payload: &Value) -> Result<Value> {
    let record = account::record()?;
    bound(payload, &record)?;
    change(|state, now| {
        let request = &state["request"];
        if !request.is_object() || request["requestId"] != payload["requestId"] { return Ok(json!({ "accepted": false, "reason": "expired" })); }
        if request["clientId"].as_str().is_none() || request["clientId"] != payload["clientId"]
            || request["planId"] != payload["planId"] || request["accountId"] != record["id"] || request["configId"] != payload["configId"] {
            return Err(Error::http(400, "验证结果与当前账号、套餐、配置或界面不匹配"));
        }
        if request["reply"].is_object() { return Ok(json!({ "accepted": false, "reason": "already_submitted" })); }
        let status = text(payload, "status");
        if !["verified", "cancelled", "timeout", "failed"].contains(&status) { return Err(Error::http(400, "领取验证结果状态无效")); }
        let param = if status == "verified" { text(payload, "param") } else { "" };
        if status == "verified" { proof(param)?; consume(state, param, now)?; }
        state["request"]["reply"] = json!({ "status": status, "param": param });
        Ok(json!({ "accepted": true }))
    })
}

// 保留旧插件内部动作接线；主程序只开放固定 complete(status=cancelled) 短通道。
pub fn cancel(payload: &Value) -> Result<Value> {
    let mut payload = payload.clone();
    payload["status"] = json!("cancelled");
    complete(&payload)
}
