//! 国际版验证码只信任上游 client/configs 的真实字段；不写死国内 scene/prefix/region。
//! 已确认的 Aliyun 形状才交给界面，缺配置不当免验证，也不假造其它验证提供商。
//! 模型票据与领取任务分开，必须绑定 provider、账号、凭据身份及本次配置。
use agent2api_wasm_sdk::{self as sdk, json, state::Snapshot, Error, Result, Value};
use super::{account, config, network::{self, text, ZCODE, PROVIDER}};

const KEY: &str = "model-captcha";
const GATE: &str = "model-captcha-gate";
const TTL: i64 = 120_000;
const CONFIG_TTL: i64 = 300_000;
const MAX_TOKENS: usize = 12;
const MAX_REPLAYS: usize = 512;

fn changed() -> Error { Error::http(409, "国际版账号或验证配置已变化，请重新获取验证码配置") }
fn refresh_required() -> Error { Error::http(428, "国际版验证码配置已过期或不存在，请重新获取配置后完成验证") }

fn configuration_id(binding: &str, version: &str, config: &Value) -> String {
    network::hash(&json!([binding, version, config["enabled"], config["prefix"], config["sceneId"],
        config["region"], config["skipModelRequest"]]).to_string())
}

fn cached(before: &Snapshot, record: &Value, binding: &str, version: &str) -> Option<Value> {
    let value = &before.value["config"];
    let until = before.value["until"].as_i64()?;
    let now = sdk::now();
    let enabled = value["enabled"].as_bool()?;
    if !before.compatible || before.value["binding"] != binding || before.value["version"] != version
        || until <= now || until > now.saturating_add(CONFIG_TTL)
        || value["provider"] != PROVIDER || value["accountId"] != record["id"]
        || value["engine"] != (if enabled { "aliyun" } else { "none" })
        || !value["skipModelRequest"].is_boolean()
        || value["configId"] != configuration_id(binding, version, value) { return None; }
    if ["prefix", "sceneId", "region"].iter().any(|key| value[*key].as_str().is_none_or(|text|
        text.len() > 256 || text.chars().any(char::is_control) || enabled && text.trim().is_empty())) { return None; }
    Some(value.clone())
}

fn current_binding(record: &Value, binding: &str) -> Result<()> {
    let current = account::record()?;
    if current["id"] != record["id"] || config::model_verification_binding(&current) != binding { return Err(changed()); }
    Ok(())
}

/// 提交通道只读现有绑定缓存，缓存失效不能在这里刷新上游。
pub(super) fn cached_configuration(record: &Value) -> Result<Value> {
    let binding = config::model_verification_binding(record);
    let version = config::app_version()?;
    let before = config::transient("captcha-config")?;
    let value = cached(&before, record, &binding, &version).ok_or_else(refresh_required)?;
    current_binding(record, &binding)?;
    Ok(value)
}

pub fn configuration(force: bool) -> Result<Value> {
    let record = account::record()?;
    let binding = config::model_verification_binding(&record);
    let before = config::transient("captcha-config")?;
    let version = config::app_version()?;
    if !force {
        if let Some(value) = cached(&before, &record, &binding, &version) {
            current_binding(&record, &binding)?;
            return Ok(value);
        }
    }
    let reply = network::request("GET", &format!("{ZCODE}/api/v1/client/configs?app_version={}&platform={}",
        network::query(&version), config::PLATFORM), json!({}), None)?;
    let data = network::envelope(reply, "ZCode 国际版验证配置查询")?;
    let captcha = &data["configs"]["captcha"];
    if !captcha.is_object() { return Err(Error::http(428, "上游未提供国际版验证配置，不能按免验证处理")); }
    let enabled = captcha["enabled"].as_bool().unwrap_or(true);
    let prefix = text(captcha, &["prefix"]);
    let scene = text(captcha, &["sceneId"]);
    let region = text(captcha, &["region"]);
    if enabled && [&prefix, &scene, &region].iter().any(|text| text.trim().is_empty()) {
        return Err(Error::http(428, "国际版上游验证配置缺少 prefix、sceneId 或 region，未套用国内默认值"));
    }
    let valid = |text: &str| text.len() <= 256 && !text.chars().any(char::is_control);
    if ![&prefix, &scene, &region].iter().all(|value| valid(value)) { return Err(Error::http(502, "ZCode 国际版验证配置格式无效")); }
    let skip = captcha["skip_model_request"].as_bool().unwrap_or(false);
    let config_id = network::hash(&json!([binding, version, enabled, prefix, scene, region, skip]).to_string());
    let result = json!({ "enabled": enabled, "prefix": prefix, "sceneId": scene, "region": region,
        "skipModelRequest": skip, "engine": if enabled { "aliyun" } else { "none" },
        "provider": PROVIDER, "accountId": record["id"], "configId": config_id });
    current_binding(&record, &binding)?;
    if config::app_version()? != version { return Err(changed()); }
    let now = sdk::now();
    if !config::put_transient("captcha-config", &before,
        json!({ "config": result, "binding": binding, "version": version, "until": now + CONFIG_TTL }), CONFIG_TTL)? {
        return Err(changed());
    }
    if !enabled { gate(false)?; cancel()?; }
    else { change(Some(&result), |_| Ok(()))?; }
    current_binding(&record, &binding)?;
    Ok(result)
}

fn remember(state: &mut Value, proof: &str, now: i64) -> Result<()> {
    let hash = network::hash(proof);
    let consumed = state["consumed"].as_array_mut().ok_or_else(|| Error::new("验证状态异常"))?;
    if !consumed.iter().any(|item| item["hash"] == hash) {
        if consumed.len() >= MAX_REPLAYS { return Err(Error::http(429, "短期验证码提交过多，请稍后重试")); }
        consumed.push(json!({ "hash": hash, "at": now }));
    }
    Ok(())
}

fn discard(state: &mut Value) -> Result<()> {
    let tokens = std::mem::replace(&mut state["tokens"], json!([]));
    for token in tokens.as_array().into_iter().flatten() { remember(state, &text(token, &["param"]), sdk::now())?; }
    Ok(())
}

fn prune(state: &mut Value, config_id: Option<&str>) -> Result<()> {
    if !state["tokens"].is_array() { state["tokens"] = json!([]); }
    if !state["consumed"].is_array() { state["consumed"] = json!([]); }
    let now = sdk::now();
    let alive = |value: &Value| value["at"].as_i64().is_some_and(|at| at <= now && now.saturating_sub(at) < TTL);
    state["consumed"].as_array_mut().unwrap().retain(alive);
    let mut keep = Vec::new();
    let mut stale = Vec::new();
    for token in state["tokens"].as_array().cloned().unwrap_or_default() {
        if !alive(&token) { continue; }
        let proof = text(&token, &["param"]);
        if proof.trim().is_empty() || proof.len() > 16_384 || proof.contains(['\r', '\n', '\0']) { continue; }
        if config_id.is_some_and(|id| token["configId"] != id) { stale.push(proof); }
        else if keep.len() < MAX_TOKENS { keep.push(token); }
    }
    state["tokens"] = json!(keep);
    for proof in stale { remember(state, &proof, now)?; }
    Ok(())
}

fn change<T>(configuration: Option<&Value>, mut action: impl FnMut(&mut Value) -> Result<T>) -> Result<T> {
    let record = account::record()?;
    let binding = config::model_verification_binding(&record);
    if let Some(configuration) = configuration {
        if cached_configuration(&record)?["configId"] != configuration["configId"] { return Err(changed()); }
    }
    for _ in 0..5 {
        let snapshot = config::transient(KEY)?;
        let mut value = snapshot.value.clone();
        if !snapshot.compatible || !value.is_object() || value["binding"] != binding {
            value = json!({ "binding": binding, "tokens": [], "consumed": [], "minted": 0, "used": 0 });
        }
        prune(&mut value, configuration.and_then(|value| value["configId"].as_str()))?;
        let result = action(&mut value)?;
        current_binding(&record, &binding)?;
        if let Some(configuration) = configuration {
            if cached_configuration(&record)?["configId"] != configuration["configId"] { return Err(changed()); }
        }
        if config::put_transient(KEY, &snapshot, value, TTL)? { return Ok(result); }
    }
    Err(Error::http(409, "ZCode 国际版验证状态正在变化，请重试"))
}

fn gate(challenged: bool) -> Result<()> {
    let record = account::record()?;
    let binding = config::model_verification_binding(&record);
    for _ in 0..5 {
        let before = config::transient(GATE)?;
        let value = json!({ "binding": binding, "challenged": challenged,
            "lastChallengeAt": if challenged { json!(sdk::now()) } else { Value::Null } });
        current_binding(&record, &binding)?;
        if config::put_transient(GATE, &before, value, 86_400_000)? { return Ok(()); }
    }
    Err(Error::http(409, "ZCode 国际版验证要求正在变化，请重试"))
}

pub fn required(configuration: &Value) -> Result<bool> {
    if sdk::settings::get("requireCaptcha")? == true { return Ok(true); }
    let binding = config::model_verification_binding(&account::record()?);
    let gate = config::transient(GATE)?;
    let challenged = gate.compatible && gate.value["binding"] == binding && gate.value["challenged"] == true;
    Ok(challenged || configuration["enabled"] != false && configuration["skipModelRequest"] != true)
}

pub fn challenge() -> Result<()> { gate(true) }

pub fn take(configuration: &Value) -> Result<Option<(String, String)>> {
    change(Some(configuration), |state| {
        state["lastDemandAt"] = json!(sdk::now());
        if configuration["enabled"] == false { return Ok(None); }
        let Some(tokens) = state["tokens"].as_array_mut() else { return Ok(None); };
        if tokens.is_empty() { return Ok(None); }
        let value = tokens.remove(0);
        let param = text(&value, &["param"]);
        remember(state, &param, sdk::now())?;
        state["used"] = json!(state["used"].as_u64().unwrap_or(0).saturating_add(1));
        Ok(Some((param, text(configuration, &["region"]))))
    })
}

pub fn push(payload: &Value) -> Result<Value> {
    let record = account::record()?;
    let configuration = cached_configuration(&record)?;
    if payload["provider"] != PROVIDER || payload["accountId"] != record["id"] || payload["configId"] != configuration["configId"] {
        return Err(Error::http(400, "模型验证结果与当前国际版账号或验证配置不匹配"));
    }
    if configuration["enabled"] == false { return Ok(json!({ "accepted": 0, "reason": "disabled" })); }
    let items = payload["tokens"].as_array().filter(|items| !items.is_empty() && items.len() <= MAX_TOKENS)
        .ok_or_else(|| Error::http(400, "单次验证令牌数量无效"))?;
    let accepted = change(Some(&configuration), |state| {
        let mut accepted = 0_u64;
        for item in items {
            let param = text(item, &["param"]);
            if param.trim().is_empty() || param.len() > 16_384 || param.contains(['\r', '\n', '\0']) { continue; }
            let hash = network::hash(&param);
            if state["tokens"].as_array().into_iter().flatten().any(|token| token["param"] == param)
                || state["consumed"].as_array().into_iter().flatten().any(|token| token["hash"] == hash) { continue; }
            if state["tokens"].as_array().map_or(0, Vec::len) >= MAX_TOKENS { break; }
            if state["consumed"].as_array().map_or(0, Vec::len) + state["tokens"].as_array().map_or(0, Vec::len) >= MAX_REPLAYS {
                return Err(Error::http(429, "短期验证码提交过多，请稍后重试"));
            }
            state["tokens"].as_array_mut().ok_or_else(|| Error::new("验证状态异常"))?
                .push(json!({ "param": param, "configId": configuration["configId"], "at": sdk::now() }));
            accepted += 1;
        }
        state["minted"] = json!(state["minted"].as_u64().unwrap_or(0).saturating_add(accepted));
        Ok(accepted)
    })?;
    Ok(json!({ "accepted": accepted }))
}

pub fn stats() -> Result<Value> {
    let record = account::record()?;
    let (required, configuration, error) = match configuration(false) {
        Ok(configuration) => (required(&configuration)?, configuration, None),
        Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
        Err(error) => (true, Value::Null, Some(error.message)),
    };
    let gate = config::transient(GATE)?;
    let binding = config::model_verification_binding(&record);
    change(configuration.is_object().then_some(&configuration), |state| {
        if configuration.is_null() { discard(state)?; }
        let last_demand = state["lastDemandAt"].as_i64().unwrap_or(0);
        let now = sdk::now();
        Ok(json!({ "provider": PROVIDER, "accountId": record["id"], "config": configuration,
            "ready": state["tokens"].as_array().map(Vec::len).unwrap_or(0), "required": required,
            "ttlMs": TTL, "minted": state["minted"], "consumed": state["used"],
            "lastChallengeAt": if gate.compatible && gate.value["binding"] == binding { gate.value["lastChallengeAt"].clone() } else { Value::Null },
            "demandActive": last_demand > 0 && last_demand <= now && now.saturating_sub(last_demand) < TTL,
            "error": error, "localGates": [] }))
    })
}

pub fn cancel() -> Result<Value> {
    change(None, |state| {
        discard(state)?;
        state["lastDemandAt"] = json!(0); Ok(Value::Null)
    })
}
