use std::collections::BTreeMap;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::{api, desktop, PROVIDER_ID};
use crate::errors::GatewayError;

const MAX_COOKIE_BYTES: usize = 64 * 1024;

/// 「导入本机登录态」是不是**存了快照**（而不是只有身份）。
///
/// ── 为什么要有这一步判断（2026-11-03 改）────────────────────
/// 早期实现只存身份、不复制 Cookie，每次请求实时去客户端读（CDP 需要
/// `--remote-debugging-port=9342`）。结果是：客户端一关，查余额、对话、
/// 签到、拉模型全都报「未连接到豆包工作客户端」。用户要的是「导入完就能
/// 像网页登录的账号那样直接用」——所以要判断这条记录里到底有没有快照。
fn snapshot(secret: &Value) -> bool {
    secret.get("cookies").is_some() || secret.get("cookieHeader").is_some()
}

/// 两侧的会话标识（`sessionid`；老记录可能只有 `sessionid_ss`）。
fn session_of(secret: &Value) -> Option<String> {
    let pairs = pairs(secret).ok()?;
    pairs
        .get("sessionid")
        .or_else(|| pairs.get("sessionid_ss"))
        .cloned()
}

/// 客户端实时读到的 Cookie（读不到就是 `None`，不把「客户端没开」当错误）。
///
/// 给 3 秒上限：客户端正常时 CDP 往返是毫秒级；客户端没开 / 没开调试端口时
/// 本机连接会被立刻拒绝。上限只兜「端口开着但页面卡住」这种极端情况，不让
/// 它把一次余额查询拖到 10 秒。
async fn live_credentials() -> Option<Value> {
    let value = tokio::time::timeout(std::time::Duration::from_secs(3), desktop::read_cdp())
        .await
        .ok()?
        .ok()?;
    credentials(&value).ok()
}

fn domain_allowed(domain: &str) -> bool {
    let domain = domain.trim_start_matches('.');
    domain == "doubao.com" || domain == "www.doubao.com"
}

fn valid_pair(name: &str, value: &str) -> bool {
    !name.is_empty()
        && !name
            .bytes()
            .any(|byte| byte.is_ascii_control() || b"=;, ".contains(&byte))
        && !value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b';')
}

fn pairs(value: &Value) -> Result<BTreeMap<String, String>, GatewayError> {
    let mut result: BTreeMap<String, String> = BTreeMap::new();
    let source = value
        .get("cookies")
        .or_else(|| value.get("cookieHeader"))
        .or_else(|| value.get("cookie"))
        .or_else(|| value.get("token"))
        .unwrap_or(value);
    match source {
        Value::String(text) => {
            let text = text
                .trim()
                .strip_prefix("Cookie:")
                .unwrap_or(text.trim())
                .trim();
            if text.len() > MAX_COOKIE_BYTES {
                return Err(GatewayError::with_status(400, "豆包工作 Cookie 过长"));
            }
            if text.starts_with('[') || text.starts_with('{') {
                let parsed: Value = serde_json::from_str(text)
                    .map_err(|_| GatewayError::with_status(400, "Cookie JSON 无法解析"))?;
                return pairs(&parsed);
            }
            for part in text
                .split(';')
                .map(str::trim)
                .filter(|part| !part.is_empty())
            {
                let Some((name, value)) = part.split_once('=') else {
                    return Err(GatewayError::with_status(
                        400,
                        "请粘贴完整 Cookie（含 sessionid=…），不能只填令牌值",
                    ));
                };
                if !valid_pair(name.trim(), value.trim()) {
                    return Err(GatewayError::with_status(400, "Cookie 含非法字符"));
                }
                result.insert(name.trim().into(), value.trim().into());
            }
        }
        Value::Array(rows) => {
            let now = chrono::Utc::now().timestamp();
            for row in rows {
                if row["domain"]
                    .as_str()
                    .is_some_and(|domain| !domain_allowed(domain))
                    || row["path"].as_str().is_some_and(|path| path != "/")
                {
                    continue;
                }
                let expires = row["expires"]
                    .as_f64()
                    .or_else(|| row["expirationDate"].as_f64());
                if expires.is_some_and(|expiry| expiry > 0.0 && expiry <= now as f64) {
                    continue;
                }
                let name = row["name"].as_str().unwrap_or("");
                let value = row["value"].as_str().unwrap_or("");
                if !valid_pair(name, value) {
                    return Err(GatewayError::with_status(400, "Cookie 列表含非法名称或值"));
                }
                result.insert(name.into(), value.into());
            }
        }
        Value::Object(object) => {
            for (name, value) in object {
                if let Some(value) = value.as_str() {
                    if valid_pair(name, value) {
                        result.insert(name.clone(), value.into());
                    }
                }
            }
        }
        _ => return Err(GatewayError::with_status(400, "缺少豆包工作 Cookie")),
    }
    if !["sessionid", "sessionid_ss"]
        .iter()
        .any(|name| result.get(*name).is_some_and(|value| !value.is_empty()))
    {
        return Err(GatewayError::with_status(
            401,
            "豆包工作 Cookie 缺少 sessionid，请重新登录后复制完整 Cookie",
        ));
    }
    let size: usize = result
        .iter()
        .map(|(name, value)| name.len() + value.len() + 3)
        .sum();
    if size > MAX_COOKIE_BYTES {
        return Err(GatewayError::with_status(400, "豆包工作 Cookie 过长"));
    }
    Ok(result)
}

pub fn cookie_header(secret: &Value) -> Result<String, GatewayError> {
    let cookies = pairs(secret)?;
    if let Some(header) = secret["cookieHeader"].as_str() {
        if header.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(GatewayError::with_status(400, "Cookie 含非法字符"));
        }
        return Ok(header.to_string());
    }
    Ok(cookies
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; "))
}

fn credentials(value: &Value) -> Result<Value, GatewayError> {
    if value["edition"]
        .as_str()
        .is_some_and(|edition| edition != PROVIDER_ID)
        || value["baseUrl"]
            .as_str()
            .is_some_and(|base| base.trim_end_matches('/') != api::BASE_URL)
    {
        return Err(GatewayError::with_status(400, "凭证不是豆包工作国内版账号"));
    }
    let pairs = pairs(value)?;
    let pick = |name: &str| pairs.get(name).cloned();
    let mut secret = json!({
        "cookieHeader":pairs.iter().map(|(name, value)|format!("{name}={value}")).collect::<Vec<_>>().join("; "),
        "deviceId":pick("device_id"), "webId":pick("web_id"),
        "fingerprint":pick("s_v_web_id").or_else(||pick("fp")),
    });
    if let Some(cookies) = value.get("cookies").filter(|cookies| cookies.is_array()) {
        secret["cookies"] = cookies.clone();
    }
    for key in [
        "name",
        "userId",
        "enterpriseType",
        "email",
        "avatar",
        "clientVersion",
        "channel",
        "userAgent",
        "chromiumVersion",
    ] {
        if let Some(field) = value.get(key) {
            secret[key] = field.clone();
        }
    }
    for key in ["deviceId", "webId", "fingerprint"] {
        if let Some(field) = value.get(key).filter(|field| field.is_string()) {
            secret[key] = field.clone();
        }
    }
    if value.get("cookies").is_some_and(Value::is_array) {
        if let Some(object) = secret.as_object_mut() {
            object.remove("cookieHeader");
        }
    }
    Ok(secret)
}

pub async fn identify(client: &reqwest::Client, secret: &Value) -> Result<Value, GatewayError> {
    let mut request = client
        .get(format!("{}/passport/account/info/v2/", api::BASE_URL))
        .query(&[
            ("account_sdk_source", "web"),
            ("sdk_version", "2.2.5"),
            ("aid", "582478"),
            ("real_aid", "582478"),
            ("device_platform", "web"),
            ("samantha_web", "1"),
            ("use-olympus-account", "1"),
        ]);
    for (key, value) in api::headers(secret)? {
        request = request.header(key, value);
    }
    let value = api::json_response(request, false).await?;
    if value["message"]
        .as_str()
        .is_some_and(|message| message != "success")
    {
        return Err(GatewayError::with_status(
            401,
            "豆包工作登录态已失效，请重新登录",
        ));
    }
    let data = value
        .get("data")
        .filter(|data| data.is_object())
        .unwrap_or(&value);
    let user = api::identifier(&data["user_id_str"])
        .ok_or_else(|| GatewayError::with_status(401, "豆包工作未返回账号身份，请重新登录"))?;
    Ok(json!({
        "userId":user, "enterpriseType":api::identifier(&data["passport_enterprise_user_type"]).unwrap_or_else(||"0".into()),
        "name":data["name"].as_str().unwrap_or(&user), "email":data["email"], "avatar":data["avatar_url"],
    }))
}

pub async fn normalize(
    ctx: &PluginCtx,
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, GatewayError> {
    let desktop = matches!(input.source, proto::NormalizeSource::Desktop);
    let value = match input.source {
        proto::NormalizeSource::Manual { payload } => payload,
        proto::NormalizeSource::FileText { content, .. } => serde_json::from_str::<Value>(&content)
            .map_err(|_| GatewayError::with_status(400, "请导入包含 Cookie 的 JSON 文件"))?,
        proto::NormalizeSource::Desktop => desktop::read().await?,
        proto::NormalizeSource::FileBase64 { .. } => {
            return Err(GatewayError::with_status(
                400,
                "豆包工作不导入加密 Cookies 数据库，请使用网页登录或粘贴 Cookie",
            ))
        }
    };
    let mut secret = credentials(&value)?;
    let client = api::client(input.proxy.as_ref(), &ctx.env.proxy, false)?;
    let profile = identify(&client, &secret).await?;
    let display = input
        .name
        .or_else(|| value["name"].as_str().map(str::to_string))
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| profile["name"].as_str().unwrap_or("豆包工作").into());
    for key in ["userId", "enterpriseType", "email", "avatar"] {
        secret[key] = profile[key].clone();
    }
    secret["name"] = json!(display);
    if desktop {
        // ── 导入即快照（2026-11-03 改）─────────────────────────────
        // 以前这里只留身份、把 Cookie 丢掉，每次请求实时读客户端；结果是
        // 客户端一关，查余额 / 对话 / 签到全都报「未连接到豆包工作客户端」。
        // 现在把读到的 Cookie 一起存下来：导入完就与网页登录的账号完全一样，
        // 客户端关着也能用。`source` 只作来源标记，`resolve` 据此决定
        // 「客户端开着时优先用最新的那份」。
        secret["source"] = json!("desktop");
        // 设备号是风控认的可信设备标识（见 `client` 模块头）。客户端没开
        // 也能从它的 `Local State` 文件里读到，顺手一起存：这样即使以后
        // 客户端被卸载，快照照样能过风控。
        if secret["deviceId"].is_null() {
            if let Some(device) = super::client::device_id() {
                secret["deviceId"] = json!(device);
            }
        }
    }
    // 只存身份的老记录才算「桌面端实时登录态」；带快照的就是普通账号。
    let reference = desktop && !snapshot(&secret);
    Ok(proto::NormalizedAccount {
        public: public(&secret)?,
        identity: identity(&secret),
        secret,
        desktop: reference,
    })
}

pub fn identity(secret: &Value) -> String {
    let user = secret["userId"].as_str().unwrap_or("");
    if user.is_empty() {
        return String::new();
    }
    format!(
        "doubaowork-{user}-{}",
        secret["enterpriseType"].as_str().unwrap_or("0")
    )
}

pub fn public(secret: &Value) -> Result<Value, GatewayError> {
    // 只有「只存身份、没有快照」的老记录才是桌面端实时登录态；
    // 新导入的记录带着 Cookie 快照，按普通账号展示（可导出、可迁移）。
    let desktop = secret["source"] == "desktop" && !snapshot(secret);
    let token_tail = if desktop {
        String::new()
    } else {
        let cookies = pairs(secret)?;
        crate::store::token_tail(
            cookies
                .get("sessionid")
                .or_else(|| cookies.get("sessionid_ss"))
                .map(String::as_str)
                .unwrap_or(""),
        )
    };
    Ok(
        json!({"name":secret["name"],"nickname":secret["name"],"userId":secret["userId"],"email":secret["email"],"avatar":secret["avatar"],"tokenTail":token_tail,"hasCredentials":true,"desktop":desktop}),
    )
}

pub async fn resolve(secret: &Value) -> Result<Value, GatewayError> {
    if secret["source"] == "desktop" {
        // ── 带快照：直接用，客户端关着也能查余额 / 对话 ─────────────
        // 这就是「导入即复制」的意义：导入完成之后，账号与一个网页登录 /
        // 粘贴 Cookie 的账号没有区别。
        //
        // 客户端开着时**只**在同一个会话时优先用实时那份（`sessionid` 相同）：
        // 不同的会话说明客户端重新登录过（可能换了账号），这时宁可用导入时的
        // 快照，也不能悄悄按客户端当前登录的账号发请求 —— 那是另一笔账。
        if snapshot(secret) {
            if let Some(live) = live_credentials().await {
                if session_of(&live).is_some() && session_of(&live) == session_of(secret) {
                    return Ok(live);
                }
                crate::logging::verbose(
                    "[DoubaoWork]",
                    "客户端当前的登录会话与导入时不同，继续使用导入时保存的登录态；重新导入可同步",
                );
            }
            return credentials(secret);
        }
        // ── 只存身份的老记录：只能实时读客户端 ─────────────────────
        let mut current = credentials(&desktop::read().await.map_err(|error| legacy_hint(error))?)
            .map_err(legacy_hint)?;
        current["expectedUserId"] = secret["userId"].clone();
        current["expectedEnterpriseType"] = secret["enterpriseType"].clone();
        return Ok(current);
    }
    credentials(secret)
}

/// 老记录读不到客户端时的补充说明：这个账号是旧版导入，重新导入一次即可脱离客户端。
fn legacy_hint(error: GatewayError) -> GatewayError {
    GatewayError::with_status(
        error.status_code,
        format!(
            "{}（这个账号是旧版导入，只存了身份、没有保存登录态；请重新「导入本机登录态」一次，之后就不需要客户端了）",
            error.message
        ),
    )
    .with_optional_code(error.upstream_code)
}

/// 把「只存身份」的老记录就地升级成带快照的记录（`api::account` 在成功拿到
/// 实时凭证后调用）。返回 `None` 表示不需要 / 拿不到 Cookie，调用方不用写回。
///
/// 这一步只改 `pluginSecret`，不动用户的优先级、启用状态等运营字段。
pub fn upgrade_snapshot(secret: &Value, current: &Value) -> Option<Value> {
    if secret["source"] != "desktop" || snapshot(secret) || !snapshot(current) {
        return None;
    }
    let mut next = secret.clone();
    for key in [
        "cookieHeader",
        "cookies",
        "deviceId",
        "webId",
        "fingerprint",
    ] {
        if let Some(value) = current.get(key).filter(|value| !value.is_null()) {
            next[key] = value.clone();
        }
    }
    // 从 Cookie 库读出来的登录态通常没有 `device_id` Cookie；设备号在客户端的
    // `Local State` 里，一并存下来，卸载客户端后这份快照也仍带可信设备标识。
    if next["deviceId"].is_null() {
        if let Some(device) = super::client::device_id() {
            next["deviceId"] = json!(device);
        }
    }
    Some(next)
}
