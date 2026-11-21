//! JWT 的 userId/exp 只作字段解析、不当作签名验证；新导入须能确认稳定账号身份。
//! 合法旧账号缺 uid 仍保留末八位兼容，公开 tokenTail 最多六位。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use base64::Engine;

pub const MAX_IMPORT_BYTES: usize = 256 * 1024;
const EXPIRY_WINDOW_SECONDS: i64 = 10 * 60;

#[derive(Clone)]
pub struct Credentials {
    pub access_token: String,
    pub expires_at: i64,
    pub user_id: String,
    pub nickname: String,
}

impl Credentials {
    pub fn is_expiring_within(&self, min_lead_ms: i64) -> bool {
        let min_seconds = if min_lead_ms > 0 {
            min_lead_ms.saturating_add(999) / 1000
        } else {
            0
        };
        let window = EXPIRY_WINDOW_SECONDS.max(min_seconds);
        self.expires_at > 0
            && self.expires_at.saturating_sub(sdk::now() / 1000) < window
    }

    pub fn is_expiring(&self) -> bool {
        self.is_expiring_within(0)
    }

    pub fn identity_tail(&self) -> String {
        tail(&self.access_token, 8)
    }
    pub fn public_tail(&self) -> String {
        tail(&self.access_token, 6)
    }

    pub fn secret(&self) -> Value {
        json!({ "accessToken": self.access_token, "tokenExpireAt": self.expires_at,
            "uid": self.user_id, "loginName": self.nickname })
    }
}

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

pub fn identifier(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| match value.get(*key) {
            Some(Value::String(text)) if !text.trim().is_empty() => Some(text.trim().to_string()),
            Some(Value::Number(number)) if number.is_i64() || number.is_u64() => {
                Some(number.to_string())
            }
            _ => None,
        })
        .unwrap_or_default()
}

fn tail(token: &str, length: usize) -> String {
    let token = token.trim();
    let chars: Vec<char> = token.chars().collect();
    // 末段先保留八位的旧身份，但公开尾号不能是完整 JWT 签名。
    if chars.len() <= length
        || token
            .rsplit('.')
            .next()
            .is_some_and(|signature| signature.chars().count() <= length)
    {
        return String::new();
    }
    chars[chars.len() - length..].iter().collect()
}

pub fn normalize_token(raw: &str) -> String {
    let mut token = raw.trim();
    loop {
        if let Some(rest) = token.strip_prefix("ot_session=") {
            token = rest.trim();
        } else if token
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Bearer "))
        {
            token = token[7..].trim();
        } else {
            break;
        }
    }
    token.to_string()
}

pub fn expiry_seconds(raw: i64) -> i64 {
    if raw >= 100_000_000_000 {
        raw / 1000
    } else {
        raw
    }
}

pub fn claims(token: &str) -> (String, i64) {
    let parse = || -> Option<Value> {
        let payload = token.split('.').nth(1)?;
        let payload = payload.trim_end_matches('=');
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .ok()?;
        let value: Value = serde_json::from_slice(&bytes).ok()?;
        value.is_object().then_some(value)
    };
    let Some(value) = parse() else {
        return (String::new(), 0);
    };
    let uid = match value.get("userId") {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    };
    (uid, value["exp"].as_i64().unwrap_or(0))
}

pub fn from_payload(payload: &Value) -> Result<Credentials> {
    let token = match payload {
        Value::String(raw) => normalize_token(raw),
        Value::Object(_) => normalize_token(&text(
            payload,
            &[
                "accessToken",
                "AccessToken",
                "access_token",
                "token",
                "oauth_access_token",
            ],
        )),
        _ => String::new(),
    };
    if token.len() > MAX_IMPORT_BYTES {
        return Err(Error::http(400, "OrcaTerm 凭据超过 256 KiB 限制"));
    }
    if token.split('.').count() != 3
        || token.split('.').any(str::is_empty)
        || token
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control() || matches!(ch, ';' | ',' | '"'))
    {
        return Err(Error::http(
            400,
            "这不是一个有效的 OrcaTerm accessToken（JWT 应为三段点分）",
        ));
    }
    let (user_id, jwt_exp) = claims(&token);
    let expires_at = if jwt_exp > 0 {
        jwt_exp
    } else {
        payload
            .get("oauth_expires_at")
            .or_else(|| payload.get("tokenExpireAt"))
            .and_then(Value::as_i64)
            .map(expiry_seconds)
            .unwrap_or(0)
    };
    let auth = match payload.get("auth_identity") {
        Some(Value::String(raw)) => serde_json::from_str(raw).unwrap_or(Value::Null),
        Some(value) => value.clone(),
        None => Value::Null,
    };
    let mut user_id = user_id;
    if user_id.is_empty() {
        user_id = identifier(payload, &["uid", "userId", "user_id"]);
    }
    if user_id.is_empty() {
        user_id = identifier(&auth, &["userId", "uid", "user_id"]);
    }
    if !user_id.is_empty() && (user_id.len() > 128 || user_id.chars().any(char::is_control)) {
        return Err(Error::http(400, "OrcaTerm 账号标识格式无效"));
    }
    let mut nickname = text(&auth, &["nickName", "username", "displayName"]);
    if nickname.is_empty() {
        nickname = text(payload, &["loginName"]);
    }
    Ok(Credentials {
        access_token: token,
        expires_at,
        user_id,
        nickname,
    })
}

pub fn from_secret(secret: &Value) -> Result<Credentials> {
    let mut credentials = from_payload(secret).map_err(|_| {
        Error::http(
            401,
            "OrcaTerm 账号缺少有效 accessToken，请重新导入或粘贴该账号",
        )
    })?;
    if let Some(expiry) = secret["tokenExpireAt"].as_i64().filter(|value| *value > 0) {
        credentials.expires_at = expiry_seconds(expiry);
    }
    let uid = identifier(secret, &["uid", "userId", "user_id"]);
    if !uid.is_empty() {
        if !credentials.user_id.is_empty() && uid != credentials.user_id {
            return Err(
                Error::http(409, "OrcaTerm 令牌身份与已保存账号不同，请重新添加正确账号")
                    .with_code("account_identity_changed"),
            );
        }
        credentials.user_id = uid;
    }
    Ok(credentials)
}

pub fn parse_text(raw: &str) -> Result<Credentials> {
    if raw.len() > MAX_IMPORT_BYTES {
        return Err(Error::http(400, "OrcaTerm 登录态文件超过 256 KiB 限制"));
    }
    let raw = raw.trim().trim_start_matches('\u{feff}').trim();
    let payload = serde_json::from_str(raw).unwrap_or_else(|_| json!(raw));
    from_payload(&payload)
}

pub fn random_uuid() -> Result<String> {
    let value = sdk::host("random.bytes", json!({ "length": 16 }))?;
    let mut bytes = base64::engine::general_purpose::STANDARD
        .decode(value.as_str().unwrap_or(""))
        .map_err(|_| Error::new("主程序未返回有效随机数"))?;
    if bytes.len() != 16 {
        return Err(Error::new("主程序返回的随机数长度无效"));
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}
