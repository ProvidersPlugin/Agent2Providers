//! 国际版旧凭据兼容。没有 userId / idPrefix 的存量数据仍按旧身份规则读，
//! 新导入始终绑定 zcode-intl-user-；不对已经可用的手填 Key 强加 OAuth 换取规则。
use super::network::{self, text, ID_PREFIX, PROVIDER};
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};
use base64::Engine;

pub const CODING: &str = "coding-plan";
pub const START: &str = "start-plan";

fn meaningful(value: &Value) -> bool {
    !value.is_null() && !value.as_str().is_some_and(|text| text.trim().is_empty())
}

pub fn decode(value: &Value) -> Result<Value> {
    let mut value = if let Some(text) = value.as_str() {
        serde_json::from_str(text)
            .map_err(|_| Error::http(400, "ZCode 国际版凭据存储不是有效 JSON"))?
    } else {
        value.clone()
    };
    for _ in 0..5 {
        if !value.is_object() {
            return Err(Error::http(400, "ZCode 国际版凭据存储必须是对象"));
        }
        validate_owner(&value)?;
        let nested = ["secret", "auth", "credentials", "data"]
            .into_iter()
            .find_map(|key| value.get(key).filter(|item| item.is_object()).cloned());
        let Some(mut nested) = nested else {
            break;
        };
        validate_owner(&nested)?;
        let groups: &[&[&str]] = &[
            &["accessToken", "access_token", "apiKey", "api_key"],
            &["jwt", "token"],
            &["oauthAccessToken", "oauth_access_token", "bizToken", "biz_token"],
            &["oauthRefreshToken", "oauth_refresh_token", "refreshToken", "refresh_token"],
            &["userId", "user_id"],
            &["deviceMid", "device_mid"],
        ];
        let outer = value
            .as_object()
            .ok_or_else(|| Error::http(400, "ZCode 国际版凭据对象无效"))?;
        let fields = nested
            .as_object_mut()
            .ok_or_else(|| Error::http(400, "ZCode 国际版嵌套凭据无效"))?;
        for (key, item) in outer {
            if ["secret", "auth", "credentials", "data"].contains(&key.as_str()) {
                continue;
            }
            if groups.iter().any(|group| {
                group.contains(&key.as_str())
                    && group
                        .iter()
                        .any(|alias| fields.get(*alias).is_some_and(meaningful))
            }) {
                continue;
            }
            if !fields.get(key).is_some_and(meaningful) {
                fields.insert(key.clone(), item.clone());
            }
        }
        value = nested;
    }
    if ["secret", "auth", "credentials", "data"]
        .iter()
        .any(|key| value[*key].is_object())
    {
        return Err(Error::http(400, "ZCode 国际版凭据嵌套层数过多"));
    }
    validate_owner(&value)?;
    Ok(value)
}

fn validate_owner(value: &Value) -> Result<()> {
    let prefix = text(value, &["idPrefix"]);
    if !prefix.is_empty() && prefix != ID_PREFIX {
        return Err(Error::http(400, "这份 ZCode 凭据不属于国际版"));
    }
    for key in ["provider", "providerId", "site", "region"] {
        let owner = text(value, &[key]);
        if !owner.is_empty()
            && ![PROVIDER, "zai", "intl", "international"].contains(&owner.as_str())
        {
            return Err(Error::http(400, "凭据所属提供商与 ZCode 国际版不符"));
        }
    }
    Ok(())
}

pub fn record() -> Result<Value> {
    let mut record = sdk::account()?;
    record["secret"] = decode(&record["secret"])?;
    Ok(record)
}

pub fn strip_bearer(value: &str) -> String {
    let mut value = value.trim();
    while value
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer "))
    {
        value = value[7..].trim();
    }
    value.to_string()
}

fn bearer(value: String) -> String {
    strip_bearer(&value)
}

pub fn access(secret: &Value) -> String {
    bearer(text(
        secret,
        &["accessToken", "access_token", "apiKey", "api_key"],
    ))
}
pub fn jwt(secret: &Value) -> String {
    bearer(text(secret, &["jwt", "token"]))
}
pub fn user(secret: &Value) -> String {
    let user = text(secret, &["userId", "user_id"]);
    if !user.is_empty() {
        return user;
    }
    ["userId", "user_id"]
        .iter()
        .find_map(|key| secret[*key].as_u64().map(|id| id.to_string()))
        .unwrap_or_default()
}
pub fn mid(secret: &Value) -> String {
    text(secret, &["deviceMid", "device_mid"])
}
pub fn plan(secret: &Value) -> &str {
    if secret["zcodePlan"] == START {
        START
    } else {
        CODING
    }
}
pub fn label(plan: &str) -> &str {
    if plan == START {
        "套餐池（Start Plan）"
    } else {
        "编码套餐（Coding Plan）"
    }
}

pub fn identity(secret: &Value) -> String {
    let id = user(secret);
    if id.is_empty() {
        String::new()
    } else {
        format!("{}{id}", text(secret, &["idPrefix"]))
    }
}

pub fn token_expired(token: &str) -> bool {
    expiry(token).is_some_and(|at| at <= sdk::now())
}

fn expiry(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?.trim_end_matches('=');
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let exp = network::number(&value["exp"])?;
    let ms = if exp > 1e12 { exp } else { exp * 1000.0 };
    (ms > 0.0 && ms <= i64::MAX as f64).then_some(ms as i64)
}

pub fn view(secret: &Value) -> Result<Value> {
    let access = access(secret);
    let jwt = jwt(secret);
    let token = if plan(secret) == START {
        &jwt
    } else if access.is_empty() {
        &jwt
    } else {
        &access
    };
    let uid = user(secret);
    let name = text(secret, &["name", "email"]);
    let name = if name.is_empty() {
        if uid.is_empty() {
            "ZCode 国际版".to_string()
        } else {
            format!("ZCode 国际版 · {uid}")
        }
    } else {
        name
    };
    if [&name, &uid].iter().any(|public| {
        [
            &access,
            &jwt,
            &strip_bearer(&text(
                secret,
                &["oauthAccessToken", "oauth_access_token", "bizToken", "biz_token"],
            )),
            &strip_bearer(&text(
                secret,
                &[
                    "oauthRefreshToken",
                    "oauth_refresh_token",
                    "refreshToken",
                    "refresh_token",
                ],
            )),
        ]
        .iter()
        .any(|token| token.len() >= 8 && public.contains(token.as_str()))
    }) {
        return Err(Error::http(400, "ZCode 公开账号字段不能包含完整凭据"));
    }
    let tail: String = token
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let mut view = json!({ "name": name, "userId": uid, "edition": "国际版", "zcodePlan": plan(secret),
        "plan": plan(secret), "planLabel": label(plan(secret)), "tokenTail": tail,
        "desktopLaunchAvailable": super::desktop_launch::is_available(secret),
        "hasSession": !token.is_empty(), "canRefresh": false, "canClaim": !jwt.is_empty() });
    if let Some(expires) = expiry(token) {
        view["expiresAt"] = json!(expires);
    }
    Ok(view)
}

pub fn normalized(secret: Value) -> Result<Value> {
    Ok(
        json!({ "identity": identity(&secret), "public": view(&secret)?, "secret": secret, "desktop": false }),
    )
}

pub fn normalize(params: &Value) -> Result<Value> {
    let source = &params["source"];
    let payload = match source["kind"].as_str() {
        Some("manual") => {
            let value = &source["payload"];
            let value = if value["inputs"].is_object() {
                &value["inputs"]
            } else {
                value
            };
            if let Some(raw) = value["credentialJson"]
                .as_str()
                .filter(|text| !text.trim().is_empty())
            {
                serde_json::from_str(raw)
                    .map_err(|_| Error::http(400, "ZCode 国际版凭据 JSON 格式错误"))?
            } else {
                value.clone()
            }
        }
        Some("file_text") => serde_json::from_str(source["content"].as_str().unwrap_or(""))
            .map_err(|_| Error::http(400, "请导入含 accessToken / jwt 的国际版 JSON 凭据"))?,
        _ => {
            return Err(Error::http(
                400,
                "ZCode 国际版请使用网页登录、手动凭据或 JSON 文本导入",
            ))
        }
    };
    let requested_method = source["payload"]["methodIndex"].as_u64();
    if requested_method.is_some_and(|index| !matches!(index, 1 | 2)) {
        return Err(Error::http(400, "ZCode 国际版凭据登录方式无效"));
    }
    let payload = decode(&payload)?;
    let access = access(&payload);
    let jwt = jwt(&payload);
    if access.is_empty() && jwt.is_empty() {
        return Err(Error::http(400, "请至少提供 accessToken 或套餐 jwt"));
    }
    for token in [&access, &jwt] {
        validate_token(token)?;
    }
    let mid = mid(&payload);
    let mid = if mid.is_empty() {
        network::uuid()?
    } else {
        mid
    };
    if !network::uuid_shape(&mid) {
        return Err(Error::http(400, "ZCode 设备标识必须是 UUID"));
    }
    let mut uid = user(&payload);
    if uid.is_empty() && !jwt.is_empty() {
        uid = super::desktop_launch::jwt_user_id(&jwt);
    }
    let mut secret = json!({ "accessToken": access, "jwt": jwt, "deviceMid": mid,
        "userId": uid, "idPrefix": ID_PREFIX, "zcodePlan": plan(&payload) });
    let oauth_access = strip_bearer(&text(
        &payload,
        &["oauthAccessToken", "oauth_access_token", "bizToken", "biz_token"],
    ));
    if !oauth_access.is_empty() {
        validate_token(&oauth_access)?;
        secret["oauthAccessToken"] = json!(oauth_access);
    }
    let oauth_refresh = strip_bearer(&text(
        &payload,
        &[
            "oauthRefreshToken",
            "oauth_refresh_token",
            "refreshToken",
            "refresh_token",
        ],
    ));
    if !oauth_refresh.is_empty() {
        validate_token(&oauth_refresh)?;
        secret["oauthRefreshToken"] = json!(oauth_refresh);
    }
    let name = text(params, &["name"]);
    let name = if name.is_empty() {
        text(&payload, &["name"])
    } else {
        name
    };
    if !name.is_empty() {
        secret["name"] = json!(name);
    }
    let email = text(&payload, &["email"]);
    if !email.is_empty() {
        secret["email"] = json!(email);
    }
    normalized(secret)
}

pub fn same_record(before: &Value, current: &Value) -> bool {
    ["id", "provider", "identity", "desktop", "secret"]
        .iter()
        .all(|key| before[*key] == current[*key])
}

pub fn unchanged(before: &Value) -> Result<()> {
    if !same_record(before, &record()?) {
        return Err(Error::http(409, "ZCode 国际版账号已变化，请重新操作"));
    }
    Ok(())
}

pub fn ensure() -> Result<String> {
    let record = record()?;
    let secret = &record["secret"];
    let token = if plan(secret) == START {
        jwt(secret)
    } else {
        access(secret)
    };
    if token.is_empty() {
        return Err(Error::http(
            400,
            format!(
                "ZCode 国际版{}凭据字段缺失，请检查账号导入内容",
                label(plan(secret))
            ),
        ));
    }
    validate_token(&token)?;
    if expiry(&token).is_some_and(|at| at <= sdk::now()) {
        return Err(
            Error::http(401, "ZCode 国际版当前套餐通道的登录态已过期，请重新登录")
                .with_code("authentication_error"),
        );
    }
    Ok(token)
}

fn validate_token(token: &str) -> Result<()> {
    if token.len() > 32 * 1024 || token.chars().any(char::is_control) {
        return Err(Error::http(400, "ZCode 凭据长度或格式无效"));
    }
    Ok(())
}

pub fn with_device() -> Result<Value> {
    let before = record()?;
    if !mid(&before["secret"]).is_empty() {
        return Ok(before);
    }
    let _lock = AccountLock::acquire()?;
    let current = sdk::account()?;
    let mut secret = decode(&current["secret"])?;
    let mut decoded = current.clone();
    decoded["secret"] = secret.clone();
    if !same_record(&before, &decoded) {
        return Err(Error::http(409, "ZCode 国际版账号已变化，未补写旧账号设备"));
    }
    if !mid(&secret).is_empty() {
        return Ok(decoded);
    }
    // 宿主 CAS 只接受对象原值；不把旧字符串原值伪装成对象比较。
    if !current["secret"].is_object() {
        return Err(Error::http(
            400,
            "旧 ZCode 国际版字符串凭据缺少设备标识，请通过 JSON 导入重新保存",
        ));
    }
    secret["deviceMid"] = json!(network::uuid()?);
    let mut public = view(&secret)?;
    if let Some(name) = current["public"]["name"].as_str() {
        public["name"] = json!(name);
    }
    let result = sdk::host(
        "account.update",
        json!({ "expect": current["secret"], "secret": secret, "public": public }),
    )?;
    if result["updated"] != true {
        return Err(Error::http(409, "ZCode 国际版账号已变化，请重试"));
    }
    let mut next = current;
    next["secret"] = secret;
    Ok(next)
}
