//! 配置 → 真人 Aliyun V2 → 上游授权地址 → 外部浏览器 → 严格官方回调 → 换码。
//! start 只发布验证步骤并立即返回，submit 不等待整个 OAuth；票据只存在当前可信调用栈。
use agent2api_wasm_sdk::{self as sdk, json, login::managed, state::{self, Scope}, Error, Result, Value};
use super::{login_host, oauth::{self, Vendor}, oauth_urls, util};

const KEY: &str = "international-oauth";
const TTL: i64 = 300_000;

fn replaced() -> Error { Error::http(409, "AutoClaw 国际版登录已取消、过期或被其他步骤替换") }
fn failed(message: &str) -> Value { json!({ "state": "failed", "message": message }) }
pub fn flow(params: &Value) -> Result<&str> {
    params["flow"].as_str().filter(|value| !value.is_empty() && util::printable(value, 256)
        && !value.bytes().any(|byte| byte.is_ascii_whitespace()))
        .ok_or_else(|| Error::http(400, "宿主未分配有效的 AutoClaw 国际版登录流程"))
}
fn current(flow: &str) -> Result<state::Snapshot> {
    let snapshot = state::get(Scope::Login, KEY, 1)?;
    if !snapshot.compatible || snapshot.value["flow"] != flow
        || snapshot.value["expiresAt"].as_i64().unwrap_or(0) <= sdk::now() { return Err(replaced()); }
    Ok(snapshot)
}
fn save(before: &state::Snapshot, next: Value) -> Result<state::Snapshot> {
    let ttl = next["expiresAt"].as_i64().unwrap_or(0).saturating_sub(sdk::now()).min(TTL);
    if ttl <= 0 || !state::compare_exchange(Scope::Login, KEY, 1, before.revision, next.clone(), ttl)? { return Err(replaced()); }
    let after = state::get(Scope::Login, KEY, 1)?;
    if !after.compatible || after.value != next { return Err(replaced()); }
    Ok(after)
}
fn clear(before: &state::Snapshot) -> Result<()> {
    if !state::compare_exchange(Scope::Login, KEY, 1, before.revision, Value::Null, 1000)? { return Err(replaced()); }
    Ok(())
}

pub fn start(params: &Value, vendor: Vendor) -> Result<Value> {
    login_host::require_vendor(vendor)?;
    let flow = flow(params)?;
    let context = login_host::context(flow, vendor)?;
    let before = state::get(Scope::Login, KEY, 1)?;
    if !before.compatible || !before.value.is_null() { return Err(replaced()); }
    let now = sdk::now();
    if now <= 0 { return Err(Error::http(503, "主程序时钟不可用，未发起国际版授权")); }
    let config = oauth::captcha_config()?;
    if config["enabled"] != true { return Err(Error::http(503, "AutoClaw 国际版上游目前未启用 OAuth，请使用凭据导入")); }
    let initial = json!({ "flow": flow, "vendor": vendor.id(), "phase": "preparing", "expiresAt": context.expires_at,
        "generation": util::random_hex(16)?, "deviceId": util::random_hex(32)? });
    let reserved = save(&before, initial)?;
    let step = managed::publish_captcha(&context.interaction, "AutoClaw 国际版登录验证",
        "请在官方阿里云组件中完成本次真人验证，成功获取授权地址后再打开外部浏览器。",
        config["prefix"].as_str().ok_or_else(replaced)?, config["sceneId"].as_str().ok_or_else(replaced)?,
        config["region"].as_str().ok_or_else(replaced)?)?;
    let mut next = reserved.value.clone();
    next["phase"] = json!("verification");
    next["interactionId"] = step["id"].clone();
    next["captchaConfigId"] = step["captcha"]["configId"].clone();
    save(&reserved, next)?;
    Ok(json!({ "flow": flow, "url": "", "poll_interval_ms": 1500,
        "instructions": "请完成当前登录步骤的阿里云验证。", "external_browser": false, "allowed_hosts": [] }))
}

pub fn poll(params: &Value) -> Result<Value> {
    let flow = flow(params)?;
    let before = current(flow)?;
    let vendor = Vendor::parse(before.value["vendor"].as_str().unwrap_or(""))?;
    login_host::context(flow, vendor)?;
    match before.value["phase"].as_str().unwrap_or("") {
        "preparing" | "verification" | "authorizing" | "waiting" | "exchanging" => Ok(sdk::login::pending(None)),
        // 当前合同不让独立 poll 消费一次性回调结果；旧缓存不能绕过原回调来源保存。
        "ready" => { clear(&before)?; Err(Error::http(409, "旧 AutoClaw 国际版授权结果不再具有当前回调来源，请重新发起登录")) }
        _ => { clear(&before)?; Ok(failed("AutoClaw 国际版登录步骤已失效，请重新发起")) }
    }
}

pub fn submit(params: &Value) -> Result<Value> {
    let flow = flow(params)?;
    let before = current(flow)?;
    let vendor = Vendor::parse(before.value["vendor"].as_str().unwrap_or(""))?;
    let context = login_host::context(flow, vendor)?;
    if before.value["phase"] != "verification" || before.value["interactionId"] != context.interaction { return Err(replaced()); }
    let input = if vendor == Vendor::Google { Some(managed::take_input()?) } else { None };
    let mut next = before.value.clone();
    next["phase"] = json!("authorizing");
    let reserved = save(&before, next)?;
    let lease = managed::open_loopback(login_host::profile(vendor)?, &context.interaction)?;
    oauth_urls::navigate(&lease.callback_url, vendor)?;
    let device = reserved.value["deviceId"].as_str().ok_or_else(replaced)?;
    let authorization = if vendor == Vendor::Zai {
        let result = sdk::host("login.autoclaw.authorization", json!({ "vendor": "zai", "leaseId": lease.lease_id, "deviceId": device }))?;
        oauth_urls::Authorization {
            url: result["url"].as_str().ok_or_else(replaced)?.to_string(),
            official_state: result["officialState"].as_str().ok_or_else(replaced)?.to_string(),
        }
    } else {
        let input = input.as_ref().ok_or_else(replaced)?;
        let captcha = &input["captcha"];
        if captcha["engine"] != "aliyun" || captcha["configId"] != before.value["captchaConfigId"] {
            return Err(Error::coded("验证码不属于本轮国际版登录配置", "forbidden"));
        }
        let param = captcha["param"].as_str().ok_or_else(replaced)?;
        oauth::request_url(vendor, &lease.callback_url, device, param, &context.authorization_origins)?
    };
    let mut next = reserved.value.clone();
    next["phase"] = json!("waiting");
    next["navigateUri"] = json!(lease.callback_url);
    next["loopbackLease"] = json!(lease.lease_id);
    next["officialState"] = json!(authorization.official_state);
    next["captchaConfigId"] = Value::Null;
    let reserved = save(&reserved, next)?;
    let step = managed::publish_browser(&context.interaction, &format!("AutoClaw 国际版 {} 授权", vendor.label()),
        "请在外部浏览器完成官方授权。远程使用时可粘贴最终完整回调地址；不要只复制授权码。",
        &authorization.url, &lease.lease_id)?;
    let mut next = reserved.value.clone();
    next["interactionId"] = step["id"].clone();
    save(&reserved, next)?;
    Ok(sdk::login::pending(None))
}

pub fn callback(params: &Value) -> Result<Value> {
    let flow = flow(params)?;
    let before = current(flow)?;
    let vendor = Vendor::parse(before.value["vendor"].as_str().unwrap_or(""))?;
    let context = login_host::context(flow, vendor)?;
    if before.value["phase"] != "waiting" || before.value["interactionId"] != context.interaction { return Err(replaced()); }
    // 有效期沿用上次读到的绝对期限，不能凭一个新的回调扩大一次性流程寿命。
    if before.value["expiresAt"].as_i64() != Some(context.expires_at) { return Err(replaced()); }
    let input = managed::take_input()?;
    let raw = input["url"].as_str().ok_or_else(|| Error::http(400, "必须提交完整官方回调，不能只提交授权码"))?;
    let navigate = before.value["navigateUri"].as_str().ok_or_else(replaced)?;
    let official = before.value["officialState"].as_str().ok_or_else(replaced)?;
    let code = match oauth_urls::callback(raw, vendor, navigate, official)? {
        oauth_urls::Callback::Code(code) => code,
        oauth_urls::Callback::Denied(detail) => {
            clear(&before)?;
            let message = if detail.is_empty() {
                "已取消或拒绝 AutoClaw 国际版官方授权".to_string()
            } else {
                format!("已取消或拒绝 AutoClaw 国际版官方授权：{detail}")
            };
            return Ok(failed(&message));
        }
    };
    let device = before.value["deviceId"].as_str().ok_or_else(replaced)?;
    let mut next = before.value.clone();
    next["phase"] = json!("exchanging");
    let reserved = save(&before, next)?;
    let result = oauth::exchange(vendor, &code, official, navigate, device);
    match result {
        Ok(account) => {
            // 与当前宿主合同一致：只能在原可信回调 Call 中交付账号。
            // 换码后先 CAS 清除原流程材料；保存仍由宿主与最终 /wait 回执确认。
            clear(&reserved)?;
            Ok(sdk::login::done(account))
        }
        Err(error) => { clear(&reserved)?; Err(error) }
    }
}

pub fn cancel(flow: &str) -> Result<()> {
    for _ in 0..3 {
        let before = state::get(Scope::Login, KEY, 1)?;
        if !before.value.is_null() && before.value["flow"] != flow { return Err(replaced()); }
        if state::compare_exchange(Scope::Login, KEY, 1, before.revision, Value::Null, 1000)? { return Ok(()); }
    }
    Err(replaced())
}
