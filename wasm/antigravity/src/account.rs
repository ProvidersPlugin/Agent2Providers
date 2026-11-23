//! 手工导入只收 Google refresh token。旧原生没有 Desktop/文件链，不虚构本机 helper。
//! 归一化会在用户实际导入时刷新校验；开发过程中不发登录请求。

use super::{credentials, oauth, project, util};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub fn identity(secret: &Value) -> String {
    let secret = credentials::fields(secret);
    let email = util::text(secret, &["email", "userId", "user_id"]);
    if !email.is_empty() {
        return format!("antigravity-{email}");
    }
    let token = util::text(secret, &["refreshToken", "refresh_token"]);
    if token.is_empty() {
        return String::new();
    }
    format!("antigravity-{}", util::fingerprint(&token))
}

pub fn view(secret: &Value) -> Value {
    let secret = credentials::fields(secret);
    let refresh = util::text(secret, &["refreshToken", "refresh_token"]);
    let access = util::text(secret, &["accessToken", "access_token"]);
    let mut view = json!({ "name": util::safe_text(&util::text(secret, &["name"]), 100),
        "userId": util::safe_text(&util::text(secret, &["userId", "email"]), 480),
        "email": util::safe_text(&util::text(secret, &["email"]), 480),
        "projectId": util::safe_text(&util::text(secret, &["projectId", "project_id"]), 1024),
        "source": util::safe_text(&util::text(secret, &["source"]), 64),
        "tokenTail": util::tail(&refresh), "hasRefreshToken": !refresh.is_empty(),
        "hasAccessToken": !access.is_empty(), "refreshTokenLongLived": true,
        "desktopLaunchAvailable": !refresh.is_empty() && secret["desktopReadOnly"] != true,
        "available": !refresh.is_empty(), "canRefresh": !refresh.is_empty(), "desktop": false });
    if let Some(at) = credentials::expiry(secret) {
        view["expiresAt"] = json!(at);
    }
    view
}

pub fn normalized(mut secret: Value, source: &str, name: Option<&str>) -> Result<Value> {
    secret = credentials::parse(&secret)?;
    secret["source"] = json!(source);
    let email = util::text(&secret, &["email"]);
    if !email.is_empty() {
        secret["userId"] = json!(email);
    }
    let name = name
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .or_else(|| (!email.is_empty()).then_some(email))
        .unwrap_or_else(|| {
            format!(
                "Antigravity …{}",
                util::tail(&util::text(&secret, &["refreshToken"]))
            )
        });
    secret["name"] = json!(util::safe_text(&name, 100));
    let identity = identity(&secret);
    if identity.is_empty() || identity.len() > 512 || identity.chars().any(char::is_control) {
        return Err(Error::http(400, "Antigravity 账号身份字段无效"));
    }
    Ok(json!({ "identity": identity, "public": view(&secret), "secret": secret, "desktop": false }))
}

fn trim_wrappers(text: &str) -> String {
    text.trim()
        .trim_matches(|ch| matches!(ch, '"' | '\'' | '`' | ',' | ';'))
        .trim()
        .to_string()
}
fn normalize_token(raw: &str) -> String {
    let mut cleaned = trim_wrappers(raw);
    for label in ["refresh_token", "refreshtoken", "refresh token", "token"] {
        if cleaned.to_ascii_lowercase().starts_with(label) {
            let tail = cleaned.get(label.len()..).unwrap_or("").trim_start();
            if let Some(value) = tail
                .strip_prefix("=>")
                .or_else(|| tail.strip_prefix('='))
                .or_else(|| tail.strip_prefix(':'))
            {
                cleaned = trim_wrappers(value);
                break;
            }
        }
    }
    if let Some(start) = cleaned.find("1//") {
        let token: String = cleaned[start..]
            .chars()
            .take_while(|ch| {
                !ch.is_whitespace() && !matches!(ch, '"' | '\'' | ',' | '}' | ']' | ';')
            })
            .collect();
        if token.len() > 3 {
            return token;
        }
    }
    cleaned.chars().filter(|ch| !ch.is_whitespace()).collect()
}

pub fn normalize(params: &Value) -> Result<Value> {
    let source = &params["source"];
    if source["kind"] != "manual"
        || source["payload"]["methodIndex"]
            .as_u64()
            .is_some_and(|index| index != 1)
    {
        return Err(Error::http(
            400,
            "Antigravity 不支持桌面或文件导入，请网页登录或粘贴续期凭据",
        ));
    }
    let payload = source["payload"]
        .get("inputs")
        .filter(|v| v.is_object())
        .unwrap_or(&source["payload"]);
    let raw = payload
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| util::text(payload, &["refreshToken", "refresh_token", "token"]));
    if raw.is_empty() || raw.chars().count() > 8192 {
        return Err(Error::http(
            400,
            "请粘贴有效长度的 Google refresh token，而非本机路径或整份文件",
        ));
    }
    let refresh = util::header(&normalize_token(&raw), 16_384)?;
    if refresh.is_empty() {
        return Err(Error::http(400, "请粘贴 Google refresh token"));
    }
    let patch = oauth::refresh(&refresh)?;
    let mut secret = json!({ "refreshToken": refresh });
    secret = credentials::merge(&secret, &patch)?;
    let token = credentials::token(&secret)?;
    let project = util::header(&util::text(payload, &["projectId", "project_id"]), 1024)?;
    let project = if project.is_empty() {
        project::discover(&token, || Ok(())).unwrap_or_default()
    } else {
        project
    };
    if !project.is_empty() {
        secret["projectId"] = json!(project);
    }
    // 保留原生手工入口：邮箱由用户填写，网页入口才会 best-effort 调 userinfo。
    let email = util::header(&util::text(payload, &["email"]), 480)?;
    if !email.is_empty() {
        secret["email"] = json!(email);
    }
    normalized(
        secret,
        "manual",
        params["name"].as_str().or_else(|| payload["name"].as_str()),
    )
}

pub fn current() -> Result<Value> {
    let record = sdk::account()?;
    credentials::parse(&record["secret"])?;
    Ok(record)
}
pub fn unchanged(secret: &Value) -> Result<()> {
    if sdk::account()?["secret"] != *secret {
        return Err(util::conflict());
    }
    Ok(())
}
pub fn update(mut record: Value, patch: &Value) -> Result<Value> {
    let next = credentials::merge(&record["secret"], patch)?;
    let parsed = credentials::parse(&next)?;
    let result = sdk::host(
        "account.update",
        json!({ "expect": record["secret"], "secret": next, "public": view(&parsed) }),
    )?;
    if result["updated"] != true {
        return Err(util::conflict());
    }
    record["secret"] = next;
    Ok(record)
}
