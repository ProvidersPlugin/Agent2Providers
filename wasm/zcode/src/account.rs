use super::network::{self, text};
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};
use base64::Engine;

pub const CODING: &str = "coding-plan";
pub const START: &str = "start-plan";

pub fn decode(value: &Value) -> Result<Value> {
    let value = if let Some(text) = value.as_str() {
        serde_json::from_str(text).map_err(|_| Error::http(400, "ZCode 凭据存储不是有效 JSON"))?
    } else {
        value.clone()
    };
    if !value.is_object() {
        return Err(Error::http(400, "ZCode 凭据存储必须是对象"));
    }
    let prefix = text(&value, &["idPrefix"]);
    if !prefix.is_empty() && prefix != "zcode-user-" {
        return Err(Error::http(400, "这份 ZCode 凭据不属于国内版"));
    }
    Ok(value)
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

pub fn access(secret: &Value) -> String {
    strip_bearer(&text(
        secret,
        &["accessToken", "access_token", "apiKey", "api_key"],
    ))
}
pub fn jwt(secret: &Value) -> String {
    strip_bearer(&text(secret, &["jwt", "token"]))
}
pub fn user(secret: &Value) -> String {
    ["userId", "user_id"]
        .iter()
        .find_map(|key| {
            let value = &secret[*key];
            value
                .as_str()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .or_else(|| value.as_u64().map(|id| id.to_string()))
        })
        .unwrap_or_default()
}
pub fn mid(secret: &Value) -> String {
    text(secret, &["deviceMid", "device_mid"])
}
pub fn plan(secret: &Value) -> &str {
    if secret["zcodePlan"].as_str().map(str::trim) == Some(START) {
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

pub fn require_current(record: &Value) -> Result<()> {
    let current = self::record()?;
    if current["id"] != record["id"]
        || current["identity"] != record["identity"]
        || current["secret"] != record["secret"]
    {
        return Err(Error::http(409, "ZCode 账号或凭据已变化，请重试当前操作"));
    }
    Ok(())
}

pub fn identity(secret: &Value) -> String {
    let id = user(secret);
    if id.is_empty() {
        String::new()
    } else {
        format!("{}{id}", text(secret, &["idPrefix"]))
    }
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
    let token = if plan(secret) == START { &jwt } else { &access };
    let uid = user(secret);
    let name = text(secret, &["name", "email"]);
    let name = if name.is_empty() {
        if uid.is_empty() {
            "ZCode 国内版".to_string()
        } else {
            format!("ZCode 国内版 · {uid}")
        }
    } else {
        name
    };
    if [&name, &uid].iter().any(|public| {
        [&access, &jwt]
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
    let mut view = json!({ "name": name, "userId": uid, "edition": "国内版", "zcodePlan": plan(secret),
        "plan": plan(secret), "planLabel": label(plan(secret)), "tokenTail": tail,
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
                    .map_err(|_| Error::http(400, "ZCode 凭据 JSON 格式错误"))?
            } else {
                value.clone()
            }
        }
        Some("file_text") => serde_json::from_str(source["content"].as_str().unwrap_or(""))
            .map_err(|_| Error::http(400, "请导入含 accessToken / jwt 的 JSON 凭据"))?,
        _ => {
            return Err(Error::http(
                400,
                "ZCode 请使用网页登录、手动凭据或 JSON 文本导入",
            ))
        }
    };
    let payload = decode(&payload)?;
    let payload = if let Some(auth) = payload
        .get("auth")
        .or_else(|| payload.get("secret"))
        .filter(|value| value.is_object() || value.is_string())
    {
        let mut auth = decode(auth)?;
        for group in [
            &["accessToken", "access_token", "apiKey", "api_key"][..],
            &["jwt", "token"][..],
            &["userId", "user_id"][..],
            &["deviceMid", "device_mid"][..],
            &["zcodePlan"][..],
            &["name"][..],
            &["email"][..],
        ] {
            if group.iter().any(|key| {
                auth.get(*key).is_some_and(|value| {
                    !value.is_null() && value.as_str().is_none_or(|text| !text.trim().is_empty())
                })
            }) {
                continue;
            }
            for key in group {
                if let Some(value) = payload.get(*key) {
                    auth[*key] = value.clone();
                }
            }
        }
        auth
    } else {
        payload
    };
    if let Some(index) = source["payload"]["methodIndex"].as_u64() {
        if !matches!(index, 1 | 2) {
            return Err(Error::http(400, "网页登录请通过授权流程发起"));
        }
    }
    if let Some(value) = payload.get("zcodePlan") {
        if !value.is_null() && !matches!(value.as_str().map(str::trim), Some(CODING | START | "")) {
            return Err(Error::http(
                400,
                "ZCode 套餐通道无效，请选择编码套餐或套餐池",
            ));
        }
    }
    let access = access(&payload);
    let jwt = jwt(&payload);
    if access.is_empty() && jwt.is_empty() {
        return Err(Error::http(400, "请至少提供 accessToken 或套餐 jwt"));
    }
    for token in [&access, &jwt] {
        if token.len() > 32 * 1024 || token.contains(['\r', '\n', '\0']) {
            return Err(Error::http(400, "ZCode 凭据长度或格式无效"));
        }
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
    let mut secret = json!({ "accessToken": access, "jwt": jwt, "deviceMid": mid,
        "userId": user(&payload), "idPrefix": "zcode-user-", "zcodePlan": plan(&payload) });
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
                "ZCode {}凭据字段缺失，请检查账号导入内容",
                label(plan(secret))
            ),
        ));
    }
    if expiry(&token).is_some_and(|at| at <= sdk::now()) {
        return Err(
            Error::http(401, "ZCode 当前套餐通道的登录态已过期，请重新登录")
                .with_code("authentication_error"),
        );
    }
    Ok(token)
}

pub fn with_device() -> Result<Value> {
    let before = record()?;
    if !mid(&before["secret"]).is_empty() {
        return Ok(before);
    }
    let _lock = AccountLock::acquire()?;
    let current = sdk::account()?;
    let mut secret = decode(&current["secret"])?;
    if !mid(&secret).is_empty() {
        let mut current = current;
        current["secret"] = secret;
        return Ok(current);
    }
    // 宿主 CAS 只接受对象原值；不能把旧字符串原值偷偷改成不同的比较对象。
    if !current["secret"].is_object() {
        return Err(Error::http(
            400,
            "旧 ZCode 字符串凭据缺少设备标识，请通过 JSON 导入重新保存",
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
        return Err(Error::http(409, "ZCode 账号已变化，请重试"));
    }
    let mut next = current;
    next["secret"] = secret;
    Ok(next)
}
