//! 账号快照、十分钟临期刷新和 CAS 回写；本机只在显式导入时读取固定文件。

use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};
use base64::Engine;

use super::{
    credentials::{self, Credentials, MAX_IMPORT_BYTES},
    errors, upstream, DESKTOP_HELPER,
};

pub fn snapshot() -> Result<Credentials> {
    let record = sdk::account()?;
    if record["provider"] != super::ID || sdk::account_disabled(&record) {
        return Err(Error::http(409, "OrcaTerm 当前账号已停用或归属变化"));
    }
    credentials::from_secret(&record["secret"])
}

fn same_identity(previous: &Credentials, current: &Credentials) -> Result<()> {
    if !previous.user_id.is_empty() && previous.user_id != current.user_id
        || previous.user_id.is_empty() && previous.access_token != current.access_token
    {
        return Err(
            Error::http(409, "OrcaTerm 当前账号已变化，未沿用旧会话或续期结果")
                .with_code("account_identity_changed"),
        );
    }
    Ok(())
}

pub fn validate_current(expected: &Credentials) -> Result<()> {
    let current = snapshot()?;
    if current.access_token == expected.access_token && expected.user_id.is_empty() {
        return Ok(());
    }
    same_identity(expected, &current)?;
    if current.access_token != expected.access_token {
        return Err(Error::http(409, "OrcaTerm 当前凭据已更新，请重新发起请求")
            .with_code("account_credentials_changed"));
    }
    Ok(())
}

pub fn identity(secret: &Value) -> Result<String> {
    let credentials = credentials::from_secret(secret)?;
    if !credentials.user_id.is_empty() {
        return Ok(format!("orcaterm-{}", credentials.user_id));
    }
    let legacy = credentials.identity_tail();
    if legacy.is_empty() {
        return Err(Error::http(400, "OrcaTerm 凭据缺少可识别的账号身份"));
    }
    Ok(format!("orcaterm-{legacy}"))
}

pub fn view(secret: &Value) -> Result<Value> {
    let credentials = credentials::from_secret(secret)?;
    let name = credentials::text(secret, &["name", "loginName"]);
    let name = if name.is_empty() {
        format!("OrcaTerm {}", credentials.public_tail())
    } else {
        name
    };
    Ok(json!({
        "name": name, "uid": credentials.user_id, "tokenTail": credentials.public_tail(),
        "tokenExpireAt": (credentials.expires_at > 0).then_some(credentials.expires_at.saturating_mul(1000)),
        "hasAccessToken": true, "hasSession": true, "canRefresh": true
    }))
}

pub fn normalized(credentials: &Credentials, name: Option<&str>, desktop: bool) -> Result<Value> {
    let mut secret = credentials.secret();
    if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
        secret["name"] = json!(name);
    }
    Ok(
        json!({ "identity": identity(&secret)?, "public": view(&secret)?, "secret": secret, "desktop": desktop }),
    )
}

pub fn bind_profile(credentials: &mut Credentials, uid: String, nickname: String) -> Result<()> {
    if !uid.is_empty() {
        if !credentials.user_id.is_empty() && credentials.user_id != uid {
            return Err(
                Error::http(409, "OrcaTerm 返回了不同账号身份，未替换当前账号")
                    .with_code("account_identity_changed"),
            );
        }
        credentials.user_id = uid;
    }
    if !nickname.is_empty() {
        credentials.nickname = nickname;
    }
    Ok(())
}

pub fn normalize(params: &Value) -> Result<Value> {
    let source = &params["source"];
    let mut credentials = match source["kind"].as_str() {
        Some("manual") => {
            let payload = &source["payload"];
            let payload = if payload["inputs"].is_object() {
                &payload["inputs"]
            } else {
                payload
            };
            if let Some(raw) = payload.as_str() {
                credentials::parse_text(raw)?
            } else if let Some(raw) = payload.get("credentialText").and_then(Value::as_str) {
                credentials::parse_text(raw)?
            } else if let Some(raw) = payload
                .get("accessToken")
                .or_else(|| payload.get("token"))
                .and_then(Value::as_str)
            {
                if raw.trim_start().starts_with(['{', '"']) {
                    credentials::parse_text(raw)?
                } else {
                    credentials::from_payload(payload)?
                }
            } else {
                credentials::from_payload(payload)?
            }
        }
        Some("file_text") => credentials::parse_text(source["content"].as_str().unwrap_or(""))?,
        Some("file_base64") => {
            let encoded = source["content"].as_str().unwrap_or("");
            if encoded.len() > MAX_IMPORT_BYTES.div_ceil(3) * 4 {
                return Err(Error::http(400, "OrcaTerm 登录态文件超过 256 KiB 限制"));
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| Error::http(400, "文件内容不是合法 base64"))?;
            if bytes.len() > MAX_IMPORT_BYTES {
                return Err(Error::http(400, "OrcaTerm 登录态文件超过 256 KiB 限制"));
            }
            let raw = String::from_utf8(bytes)
                .map_err(|_| Error::http(400, "data.bin 不是 UTF-8 文本"))?;
            credentials::parse_text(&raw)?
        }
        Some("desktop") => {
            if source
                .as_object()
                .is_none_or(|object| object.keys().any(|key| key != "kind"))
            {
                return Err(Error::http(
                    400,
                    "OrcaTerm 本机导入不接受文件路径或其他读取参数",
                ));
            }
            // 本机只读通道仅在此次明确导入的宿主作用域内可用。
            let result = sdk::native_call(DESKTOP_HELPER, "read", json!({}))?;
            let raw = result["content"]
                .as_str()
                .filter(|raw| !raw.is_empty())
                .ok_or_else(|| Error::new("OrcaTerm 固定本机读取未返回登录态内容"))?;
            if raw.len() > MAX_IMPORT_BYTES {
                return Err(Error::new("OrcaTerm 本机登录态超过 256 KiB"));
            }
            credentials::parse_text(raw)?
        }
        _ => return Err(Error::http(400, "OrcaTerm 不支持该凭据来源")),
    };
    // 保留原生 best-effort 探活：旧 token 可能刚过期，添加后可立即续期。
    if let Ok((uid, nickname)) = upstream::user_info(&credentials) {
        bind_profile(&mut credentials, uid, nickname)?;
    }
    if credentials.user_id.is_empty() {
        return Err(Error::http(
            400,
            "OrcaTerm 凭据未能确认账号身份，请重新获取当前账号登录态",
        ));
    }
    // desktop 仅保留原本机导入来源；账号持有快照，普通调用不会重新读官方文件。
    normalized(
        &credentials,
        params["name"].as_str(),
        source["kind"] == "desktop",
    )
}

pub fn ensure() -> Result<Credentials> {
    let previous = snapshot()?;
    renew(&previous, false)
}

pub fn refresh() -> Result<Credentials> {
    renew(&snapshot()?, true)
}

pub fn renew(previous: &Credentials, force: bool) -> Result<Credentials> {
    if !force && !previous.is_expiring() {
        return Ok(previous.clone());
    }
    let _lock = AccountLock::acquire()?;
    let record = sdk::account()?;
    if record["provider"] != super::ID || sdk::account_disabled(&record) {
        return Err(Error::http(409, "OrcaTerm 当前账号已停用或归属变化"));
    }
    let current = credentials::from_secret(&record["secret"])?;
    same_identity(previous, &current)?;
    if !current.is_expiring() && (current.access_token != previous.access_token || !force) {
        return Ok(current);
    }
    let response = upstream::cgi(
        Some(&current),
        "OAuthRefreshToken",
        json!({ "accessToken": current.access_token }),
    )?;
    if errors::business_failed(&response) {
        return Err(errors::upstream(200, &response, &[&current.access_token]));
    }
    let token = credentials::normalize_token(&credentials::text(
        &response,
        &["AccessToken", "accessToken"],
    ));
    if token.is_empty() {
        return Err(Error::http(502, "OrcaTerm 续期响应缺少 AccessToken"));
    }
    let parsed = credentials::from_payload(&json!(token))?;
    let (uid, expires) = (parsed.user_id, parsed.expires_at);
    if !uid.is_empty() && !current.user_id.is_empty() && uid != current.user_id {
        return Err(
            Error::http(409, "OrcaTerm 续期返回了不同账号，未写入原账号")
                .with_code("account_identity_changed"),
        );
    }
    let next = Credentials {
        access_token: token,
        expires_at: if expires > 0 {
            expires
        } else {
            response["ExpiresIn"]
                .as_i64()
                .filter(|seconds| *seconds > 0)
                .map(|seconds| (sdk::now() / 1000).saturating_add(seconds))
                .unwrap_or(0)
        },
        user_id: if uid.is_empty() {
            current.user_id.clone()
        } else {
            uid
        },
        nickname: current.nickname.clone(),
    };
    let mut secret = record["secret"].clone();
    if !secret.is_object() {
        return Err(Error::new("OrcaTerm 账号凭据不是有效对象"));
    }
    for (key, value) in next.secret().as_object().into_iter().flatten() {
        secret[key] = value.clone();
    }
    // 旧原生账号的备注可能只在 public，续期更新公开视图时不能把它覆盖成昵称。
    let public_name = credentials::text(&record["public"], &["name"]);
    if !public_name.is_empty() {
        secret["name"] = json!(public_name);
    }
    let result = sdk::host(
        "account.update",
        json!({
            "expect": record["secret"], "secret": secret, "public": view(&secret)?
        }),
    )?;
    if result["updated"] == true {
        return Ok(next);
    }
    let latest = snapshot()?;
    same_identity(&current, &latest)?;
    if latest.access_token == next.access_token && latest.expires_at == next.expires_at
        || !latest.is_expiring() && latest.access_token != current.access_token
    {
        return Ok(latest);
    }
    Err(Error::new("OrcaTerm 续期结果未能落盘，请稍后重试"))
}
