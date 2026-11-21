//! 续期前置判定与公开投影；实际换证只由原宿主调用读取Ready并持有一次性工作。
//! 原生OAuth凭据依赖原client_id、PKCE verifier和DPoP私钥；ticket临时凭据不带refresh_token。

use agent2api_wasm_sdk::{
    host, http::open_raw_timeout, json, AccountLock, Error, Result, Value,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::DateTime;

use crate::{
    account,
    binding::Account,
    credentials::{Credential, RefreshMaterial},
    dpop, util,
};

pub const LEAD_MS: i64 = 15 * 60 * 1000;
const TOKEN_URL: &str = "https://sts.cn-north-4.myhuaweicloud.com/v1/oauth2/tokens";
const REFRESH_TIMEOUT_MS: u64 = 30_000;
const RELOGIN: &str = "CodeArts 登录授权此前已被明确标记失效，请通过原授权渠道重新登录；临时访问凭据续期不会延长登录授权的截止时间";
const NO_REFRESH: &str = "刷新失败，缺少 refresh_token，请通过「CodeArts 网页登录」完成授权。";

/// 按需或强制刷新 CodeArts 临时 STS 访问凭据（ES256 DPoP + PKCE refresh_token）。
/// 非强制且旧 STS 尚未过期时，若缺少续期材料或遇到上游临时故障，可保留未过期旧值继续服务；
/// 若授权已永久失效或已过期 / 强制刷新，必须返回明确失败，绝不返回旧值冒充成功。
pub fn ensure(force: bool) -> Result<String> {
    let account = if force {
        Account::current_refresh()?
    } else {
        Account::current()?
    };
    if account.needs_relogin {
        return Err(blocked(&account.credential, true));
    }
    let now = util::now()?;
    if !force && !account.credential.needs_refresh(LEAD_MS, now) {
        account.unchanged()?;
        account.readable()?;
        return Ok(account.credential.access_key_id);
    }

    let _lock = AccountLock::acquire()?;
    let latest = Account::current_refresh()?;
    latest.unchanged_refresh()?;
    if latest.needs_relogin {
        return Err(blocked(&latest.credential, true));
    }
    let now = util::now()?;
    if !force && !latest.credential.needs_refresh(LEAD_MS, now) {
        latest.readable()?;
        return Ok(latest.credential.access_key_id);
    }
    let fallback = if !force
        && latest.credential.require_readable(now).is_ok()
        && !latest.credential.needs_refresh(0, now)
    {
        Some(latest.credential.access_key_id.clone())
    } else {
        None
    };

    if latest.credential.refresh_material() != RefreshMaterial::Complete {
        if let Some(ak) = fallback {
            return Ok(ak);
        }
        return Err(blocked(&latest.credential, false));
    }

    match refresh_locked(&latest, force) {
        Ok(ak) => Ok(ak),
        Err(err) if !is_permanent_relogin(&err) => {
            if let Some(ak) = fallback {
                Ok(ak)
            } else {
                Err(err)
            }
        }
        Err(err) => Err(err),
    }
}

/// 仅回答 15 分钟临期策略；只要具有完整刷新材料即可在临期时由插件自动续期。
pub fn expiring() -> Result<bool> {
    let account = Account::current_refresh()?;
    account.unchanged_refresh()?;
    Ok(!account.needs_relogin
        && account.credential.refresh_material_complete()
        && account.credential.needs_refresh(LEAD_MS, util::now()?))
}

pub fn status() -> Result<Value> {
    let account = Account::current_refresh()?;
    account.unchanged_refresh()?;
    Ok(projection(
        &account.credential,
        account.needs_relogin,
        util::now()?,
    ))
}

pub fn projection(credential: &Credential, needs_relogin: bool, now: i64) -> Value {
    let material = credential.refresh_material();
    let expiry = credential.expiry();
    let expiry_state = match expiry {
        Some(at) if at <= now => "expired",
        Some(at) if at <= now.saturating_add(LEAD_MS) => "expiring",
        Some(_) => "valid",
        None if credential.expires_at.is_empty() => "missing",
        None => "invalid",
    };
    let (state, message) = reason(credential, needs_relogin);
    let due = if credential.expires_at.is_empty() || credential.security_token.is_empty() {
        None
    } else {
        Some(credential.needs_refresh(LEAD_MS, now))
    };
    json!({
        "canRefresh": material == RefreshMaterial::Complete && !needs_relogin,
        "refreshStatus": state,
        "refreshMessage": if state == "ready" { Value::Null } else { json!(message) },
        "refreshMaterialComplete": material == RefreshMaterial::Complete,
        "refreshMaterialStatus": material.name(),
        "refreshRequiresHost": false,
        "refreshDue": due,
        "refreshLeadMs": LEAD_MS,
        "accessExpiryStatus": expiry_state,
        "accessTokenExpiresAt": expiry,
        "refreshTokenExpiresAt": authorization_expiry(&credential.refresh_token),
        "refreshExpiryEvidence": "jwt_claim_display_only",
        "refreshCredentialKind": if credential.refresh_token.is_empty() {
            "temporary_without_refresh"
        } else { "oauth_refresh" },
        "refreshIdentityStable": credential.stable_identity().is_some(),
        "needsRelogin": needs_relogin,
        "needsReloginReason": if needs_relogin { Some(RELOGIN) } else { None },
        "oldAccessUnexpired": expiry.map(|at| at > now),
        "refreshRequestSent": false,
        "scope": "saved_credential_snapshot_only"
    })
}

fn reason(credential: &Credential, needs_relogin: bool) -> (&'static str, String) {
    if needs_relogin {
        return ("authorization_expired", RELOGIN.to_string());
    }
    let material = credential.refresh_material();
    if material == RefreshMaterial::MissingToken {
        return ("requires_relogin", NO_REFRESH.to_string());
    }
    let Some(context) = &credential.oauth_context else {
        return field_reason("missing_oauth_context", "oauth_context", true);
    };
    match material {
        RefreshMaterial::InvalidClient => field_reason(
            "invalid_client_id",
            "client_id",
            context.client_id.as_deref().is_none_or(|value| value.trim().is_empty()),
        ),
        RefreshMaterial::InvalidVerifier => {
            let (field, missing) = material_field([
                ("code_verifier", context.pkce_pair.code_verifier.as_str()),
                ("code_challenge", context.pkce_pair.code_challenge.as_str()),
                ("code_challenge_method", context.pkce_pair.code_challenge_method.as_str()),
            ], "pkce_pair");
            field_reason("invalid_pkce_verifier", field, missing)
        }
        RefreshMaterial::InvalidProofKey => {
            let private = &context.dpop_key_pair.private_key_jwk;
            let public = &context.dpop_key_pair.public_key_jwk;
            let (field, missing) = material_field([
                ("private_key_jwk.kty", private.kty.as_str()),
                ("private_key_jwk.crv", private.crv.as_str()),
                ("private_key_jwk.d", private.d.as_str()),
                ("private_key_jwk.x", private.x.as_str()),
                ("private_key_jwk.y", private.y.as_str()),
                ("public_key_jwk.kty", public.kty.as_str()),
                ("public_key_jwk.crv", public.crv.as_str()),
                ("public_key_jwk.x", public.x.as_str()),
                ("public_key_jwk.y", public.y.as_str()),
            ], "dpop_key_pair");
            field_reason("invalid_dpop_key", field, missing)
        }
        RefreshMaterial::Complete => ("ready", String::new()),
        RefreshMaterial::MissingToken => ("requires_relogin", NO_REFRESH.to_string()),
        RefreshMaterial::MissingContext => field_reason("missing_oauth_context", "oauth_context", true),
    }
}

fn material_field<const N: usize>(
    fields: [(&'static str, &str); N],
    invalid: &'static str,
) -> (&'static str, bool) {
    fields.into_iter().find(|(_, value)| value.trim().is_empty())
        .map_or((invalid, false), |(field, _)| (field, true))
}

fn field_reason(state: &'static str, field: &str, missing: bool) -> (&'static str, String) {
    let issue = if missing { format!("缺少 {field}") } else { format!("{field} 无效") };
    (state, format!("刷新失败，{issue}，请通过「CodeArts 网页登录」完成授权。"))
}

fn blocked(credential: &Credential, needs_relogin: bool) -> Error {
    let (state, message) = reason(credential, needs_relogin);
    match state {
        "authorization_expired" => Error::http(400, message).with_code("refresh_token_expired"),
        "requires_relogin" => Error::http(400, message).with_code("codearts_refresh_token_missing"),
        _ => Error::http(400, message).with_code("codearts_refresh_material_invalid"),
    }
}

fn is_permanent_relogin(err: &Error) -> bool {
    err.code.as_deref() == Some("refresh_token_expired")
}

fn refresh_locked(account: &Account, force: bool) -> Result<String> {
    let context = account
        .credential
        .oauth_context
        .as_ref()
        .ok_or_else(|| blocked(&account.credential, false))?;
    let client_id = context
        .client_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| blocked(&account.credential, false))?;
    let verifier = context.pkce_pair.code_verifier.trim();
    let refresh_token = account.credential.refresh_token.trim();
    if verifier.is_empty() || refresh_token.is_empty() {
        return Err(blocked(&account.credential, false));
    }

    let proof = dpop::token_proof(&context.dpop_key_pair)?;
    let form = encode_form(&[
        ("client_id", client_id),
        ("code_verifier", verifier),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
    ]);
    let headers = json!({
        "Content-Type": "application/x-www-form-urlencoded",
        "Accept": "application/json",
        "DPoP": proof,
    });
    let response = open_raw_timeout("POST", TOKEN_URL, headers, Some(form.as_bytes()), REFRESH_TIMEOUT_MS)?;
    let status = response.status;
    let body_bytes = response.bytes(512 * 1024)?;
    let parsed: Option<Value> = serde_json::from_slice(&body_bytes).ok();

    if status == 400 && is_sts_refresh_expired(parsed.as_ref()) {
        mark_needs_relogin(account);
        return Err(Error::http(400, RELOGIN).with_code("refresh_token_expired"));
    }

    if !(200..300).contains(&status) {
        let detail = upstream_error_summary(status, parsed.as_ref(), &body_bytes);
        return Err(Error::http(status.max(400), detail));
    }

    let payload = parsed.ok_or_else(|| Error::http(502, "CodeArts 续期响应不是有效的 JSON"))?;
    let creds = payload
        .get("credentials")
        .filter(|value| value.is_object())
        .unwrap_or(&payload);

    let access_key_id = non_empty_str(creds.get("access_key_id"))
        .ok_or_else(|| Error::http(502, "CodeArts 续期响应缺少 access_key_id"))?;
    let secret_access_key = non_empty_str(creds.get("secret_access_key"))
        .ok_or_else(|| Error::http(502, "CodeArts 续期响应缺少 secret_access_key"))?;
    let security_token = non_empty_str(creds.get("security_token"))
        .ok_or_else(|| Error::http(502, "CodeArts 续期响应缺少 security_token"))?;
    let expiration = non_empty_str(creds.get("expiration"))
        .ok_or_else(|| Error::http(502, "CodeArts 续期响应缺少 expiration"))?;
    let new_refresh_token = non_empty_str(
        payload
            .get("refresh_token")
            .or_else(|| creds.get("refresh_token")),
    )
    .ok_or_else(|| Error::http(502, "CodeArts 续期响应未返回新的 refresh_token"))?;

    let parsed_exp = DateTime::parse_from_rfc3339(expiration)
        .map_err(|_| Error::http(502, "CodeArts 续期响应的 expiration 格式无效"))?;
    let expires_at_ms = parsed_exp.timestamp_millis();
    if expires_at_ms <= util::now()? {
        return Err(Error::http(502, "CodeArts 续期响应返回了已过期的访问凭据"));
    }

    let mut next_secret = account.raw_secret.clone();
    apply_refreshed_tokens(
        &mut next_secret,
        access_key_id,
        secret_access_key,
        security_token,
        expiration,
        expires_at_ms,
        new_refresh_token,
    );

    let mut next_public = account::view(&next_secret)?;
    next_public["needsRelogin"] = json!(false);
    next_public["needsReloginReason"] = Value::Null;

    let update_result = host(
        "account.update",
        json!({
            "expect": account.raw_secret,
            "secret": next_secret,
            "public": next_public,
        }),
    );

    match update_result {
        Ok(value) if value.get("updated").and_then(Value::as_bool) == Some(true) => {
            Ok(access_key_id.to_string())
        }
        Ok(_) | Err(_) if !force => Ok(access_key_id.to_string()),
        Ok(_) => Err(Error::http(
            409,
            "账号凭据在续期期间已被外部修改，本次新令牌未覆盖写回；请重试或重新导入",
        )),
        Err(err) => Err(err),
    }
}

fn mark_needs_relogin(account: &Account) {
    let mut next_secret = account.raw_secret.clone();
    if let Some(map) = next_secret.as_object_mut() {
        map.insert("needsRelogin".into(), json!(true));
        map.insert("needsReloginReason".into(), json!(RELOGIN));
    }
    let mut next_public = account::view(&next_secret).unwrap_or_else(|_| json!({}));
    if let Some(map) = next_public.as_object_mut() {
        map.insert("canRefresh".into(), json!(false));
        map.insert("needsRelogin".into(), json!(true));
        map.insert("needsReloginReason".into(), json!(RELOGIN));
        map.insert("refreshStatus".into(), json!("authorization_expired"));
        map.insert("refreshMessage".into(), json!(RELOGIN));
    }
    let _ = host(
        "account.update",
        json!({
            "expect": account.raw_secret,
            "secret": next_secret,
            "public": next_public,
        }),
    );
}

fn apply_refreshed_tokens(
    secret: &mut Value,
    access_key_id: &str,
    secret_access_key: &str,
    security_token: &str,
    expiration: &str,
    expires_at_ms: i64,
    refresh_token: &str,
) {
    let patch = |map: &mut serde_json::Map<String, Value>| {
        map.insert("access_key_id".into(), json!(access_key_id));
        map.insert("accessToken".into(), json!(access_key_id));
        map.insert("secret_access_key".into(), json!(secret_access_key));
        map.insert("security_token".into(), json!(security_token));
        map.insert("refresh_token".into(), json!(refresh_token));
        map.insert("refreshToken".into(), json!(refresh_token));
        map.insert("expires_at".into(), json!(expiration));
        map.insert("expiration".into(), json!(expiration));
        map.insert("expiresAt".into(), json!(expires_at_ms));
        map.remove("needsRelogin");
        map.remove("needsReloginReason");
    };
    if let Some(root) = secret.as_object_mut() {
        patch(root);
        for key in ["codearts_provider_credential", "credential", "auth"] {
            if let Some(Value::Object(nested)) = root.get_mut(key) {
                patch(nested);
            }
        }
    }
}

fn is_sts_refresh_expired(body: Option<&Value>) -> bool {
    let Some(body) = body else {
        return false;
    };
    let code = body.get("error_code").and_then(Value::as_str).unwrap_or("");
    let msg = body
        .get("error_msg")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    code == "STS5.1806" && msg.eq_ignore_ascii_case("invalid refresh token: 'refresh code has expired'")
}

fn upstream_error_summary(status: u16, parsed: Option<&Value>, bytes: &[u8]) -> String {
    if let Some(body) = parsed {
        let code = body.get("error_code").and_then(Value::as_str).unwrap_or("");
        let msg = body
            .get("error_msg")
            .or_else(|| body.get("error_description"))
            .or_else(|| body.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if !code.is_empty() && !msg.is_empty() {
            return format!("CodeArts 续期失败（HTTP {status} / {code}）：{msg}");
        }
        if !msg.is_empty() {
            return format!("CodeArts 续期失败（HTTP {status}）：{msg}");
        }
    }
    let raw = String::from_utf8_lossy(bytes);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        format!("CodeArts 续期失败（HTTP {status}）")
    } else {
        let brief: String = trimmed.chars().take(180).collect();
        format!("CodeArts 续期失败（HTTP {status}）：{brief}")
    }
}

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn encode_form(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (idx, (k, v)) in pairs.iter().enumerate() {
        if idx > 0 {
            out.push('&');
        }
        push_encoded(&mut out, k);
        out.push('=');
        push_encoded(&mut out, v);
    }
    out
}

fn push_encoded(out: &mut String, input: &str) {
    for byte in input.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            b' ' => out.push('+'),
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
}

/// JWT未验签，仅展示原生已使用的exp字段；不把这个值当永久失效证据。
pub fn authorization_expiry(token: &str) -> Option<i64> {
    let mut parts = token.split('.');
    let header = parts.next()?;
    let payload = parts.next()?;
    let signature = parts.next()?;
    if header.is_empty()
        || payload.is_empty()
        || signature.is_empty()
        || parts.next().is_some()
        || token.len() > 8192
    {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims["exp"]
        .as_i64()?
        .checked_mul(1000)
        .filter(|at| *at > 0 && chrono::DateTime::from_timestamp_millis(*at).is_some())
}
