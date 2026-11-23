//! 兼容 ms- 令牌别名与有界 JSON 容器；新导入必须在线确认身份和所属站点。
//! 已存在的旧账号缺少 userId 时，保留原实现的令牌尾号身份兜底，不改变落盘契约。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

use super::{
    checkin::LOGIN_INFO_PATH,
    client::{self, Client},
    Region,
};

pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| {
            value
                .get(*key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
        })
        .unwrap_or("")
        .to_string()
}

fn token_from_payload(payload: &Value) -> String {
    let mut token = text(
        payload,
        &[
            "accessToken",
            "access_token",
            "apiKey",
            "api_key",
            "token",
            "session",
        ],
    );
    while token
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer "))
    {
        token = token[7..].trim().to_string();
    }
    token
}

pub fn user_id(value: &Value) -> String {
    ["userId", "havanaId", "HavanaId", "havana_id", "user_id"]
        .iter()
        .find_map(|key| match value.get(*key) {
            Some(Value::String(value)) if !value.trim().is_empty() => {
                Some(value.trim().to_string())
            }
            Some(value) => value.as_u64().map(|id| id.to_string()),
            None => None,
        })
        .unwrap_or_default()
}

/// 只保留当前宿主行用于原值核对，不把请求中的 account / identity 当作许可。
#[derive(Clone)]
pub struct Snapshot {
    pub record: Value,
    pub token: String,
}

impl Snapshot {
    pub fn read(region: Region) -> Result<Self> {
        let record = sdk::account()?;
        if record["id"].as_str().is_none_or(str::is_empty)
            || record["provider"].as_str() != Some(region.provider_id())
        {
            return Err(Error::http(409, "ModelScope 当前账号不属于所选站点"));
        }
        let token = token(&record["secret"])?;
        Ok(Self { record, token })
    }

    pub fn check(&self) -> Result<()> {
        let current = sdk::account()?;
        if ["id", "provider", "identity", "secret", "desktop"]
            .iter()
            .any(|key| current[*key] != self.record[*key])
        {
            return Err(
                Error::http(409, "ModelScope 账号或凭据已变化，未继续旧账号操作")
                    .with_code("account_changed"),
            );
        }
        Ok(())
    }
}

pub fn token(secret: &Value) -> Result<String> {
    let token = token_from_payload(secret);
    if token.is_empty() {
        Err(Error::http(401, "ModelScope 账号缺少令牌，请重新粘贴"))
    } else if token.len() > 8192
        || !token.is_ascii()
        || token.bytes().any(|byte| {
            byte.is_ascii_whitespace() || byte.is_ascii_control() || matches!(byte, b';' | b',')
        })
    {
        Err(Error::http(
            400,
            "ModelScope 令牌格式无效，请只粘贴访问令牌，不要粘贴整段 Cookie",
        ))
    } else {
        Ok(token)
    }
}

pub fn token_tail(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    chars[chars.len().saturating_sub(4)..].iter().collect()
}

pub fn ensure() -> Result<String> {
    token(&sdk::account()?["secret"])
}

pub fn identity(region: Region, secret: &Value) -> Result<String> {
    let user_id = user_id(secret);
    let identity = if user_id.is_empty() {
        token_tail(&token(secret)?)
    } else {
        user_id
    };
    Ok(format!("{}{identity}", region.account_prefix()))
}

pub fn view(region: Region, secret: &Value) -> Value {
    let token = token_from_payload(secret);
    let name = text(secret, &["name"]);
    let user_id = user_id(secret);
    json!({
        "name": if name.is_empty() || (!token.is_empty() && name.contains(&token)) {
            format!("ModelScope {} {}", region.label(), token_tail(&token))
        } else { name },
        "uid": user_id, "userId": user_id, "tokenTail": token_tail(&token),
        "edition": region.label(), "hasAccessToken": !token.is_empty(),
        "hasSession": !token.is_empty(), "canRefresh": false
    })
}

pub fn normalize(region: Region, params: &Value) -> Result<Value> {
    let source = &params["source"];
    let (payload, file) = match source["kind"].as_str() {
        Some("manual") => {
            let payload = &source["payload"];
            if let Some(index) = payload.get("methodIndex") {
                if !index.as_u64().is_some_and(|index| index <= 1) {
                    return Err(Error::http(400, "ModelScope 登录方式已变化，请重新选择"));
                }
            }
            let payload = payload
                .get("inputs")
                .filter(|value| value.is_object())
                .unwrap_or(payload);
            (payload.clone(), false)
        }
        Some("file_text") => {
            let raw = source["content"].as_str().unwrap_or("").trim();
            let payload =
                serde_json::from_str(raw).unwrap_or_else(|_| json!({ "accessToken": raw }));
            (payload, true)
        }
        Some("file_base64") => {
            return Err(Error::http(
                400,
                "ModelScope 不支持二进制文件导入，请粘贴 ms- 令牌",
            ))
        }
        Some("desktop") => {
            return Err(Error::http(
                400,
                "ModelScope 不支持导入桌面端登录态，请粘贴 ms- 令牌添加账号",
            ))
        }
        _ => return Err(Error::http(400, "ModelScope 请粘贴 ms- 令牌或导入文本凭证")),
    };
    let mut payload = match payload
        .get("credentialText")
        .or_else(|| payload.get("credentialJson"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        Some(raw) => serde_json::from_str(raw).unwrap_or_else(|_| json!({ "accessToken": raw })),
        None => payload,
    };
    for _ in 0..3 {
        let nested = ["secret", "pluginSecret", "auth"]
            .iter()
            .find_map(|key| payload.get(*key).filter(|value| value.is_object()).cloned());
        if token_from_payload(&payload).is_empty() {
            if let Some(nested) = nested {
                payload = nested;
                continue;
            }
        }
        break;
    }
    let secret = if let Some(raw) = payload.as_str() {
        json!({ "accessToken": raw.trim() })
    } else {
        payload.clone()
    };
    let token = token(&secret)?;
    let api = Client::new(region, token.clone())?;
    // 添加账号是显式登录；余额、目录与普通对话不会读取这个带日常发放副作用的入口。
    let response = api.site_json(LOGIN_INFO_PATH, "令牌校验")?;
    let data = client::data_of(&response);
    let user_id = user_id(&data);
    if user_id.is_empty() {
        return Err(Error::http(502, format!(
            "ModelScope 令牌校验通过但读不到账号身份（HavanaId），可能上游改了响应形态；{}站点的令牌才有效",
            region.label())));
    }
    let mut name = text(params, &["name"]);
    if name.is_empty() && !file {
        name = text(&payload, &["name"]);
    }
    if name.contains(&token) {
        name.clear();
    }
    if name.is_empty() {
        name = text(
            &data,
            &["NickName", "FirstName", "Name", "nickname", "name"],
        );
    }
    if name.is_empty() || name.contains(&token) {
        name = format!("ModelScope {} {}", region.label(), token_tail(&token));
    }
    let secret = json!({ "accessToken": token, "userId": user_id, "name": name });
    Ok(json!({
        "identity": identity(region, &secret)?, "public": view(region, &secret),
        "secret": secret, "desktop": false
    }))
}
