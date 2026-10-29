use std::collections::BTreeMap;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::{api, desktop, PROVIDER_ID};
use crate::errors::GatewayError;

const MAX_COOKIE_BYTES: usize = 64 * 1024;

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
        secret = json!({"source":"desktop","userId":secret["userId"],"enterpriseType":secret["enterpriseType"],"name":display,"email":secret["email"],"avatar":secret["avatar"]});
    }
    Ok(proto::NormalizedAccount {
        public: public(&secret)?,
        identity: identity(&secret),
        secret,
        desktop,
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
    let desktop = secret["source"] == "desktop";
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
        let mut current = credentials(&desktop::read().await?)?;
        current["expectedUserId"] = secret["userId"].clone();
        current["expectedEnterpriseType"] = secret["enterpriseType"].clone();
        return Ok(current);
    }
    credentials(secret)
}
