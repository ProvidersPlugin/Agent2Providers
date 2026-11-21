//! 续期前置判定与公开投影；实际换证只由原宿主调用读取Ready并持有一次性工作。
//! 原生OAuth凭据依赖原client_id、PKCE verifier和DPoP私钥；ticket临时凭据不带refresh_token。

use agent2api_wasm_sdk::{json, Error, Result, Value};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

use crate::{
    binding::Account,
    credentials::{Credential, RefreshMaterial},
    gates, util,
};

pub const LEAD_MS: i64 = 15 * 60 * 1000;
const RELOGIN: &str = "CodeArts 登录授权此前已被明确标记失效，请通过原授权渠道重新登录；临时访问凭据续期不会延长登录授权的截止时间";
const NO_REFRESH: &str = "刷新失败，缺少 refresh_token，请通过「CodeArts 网页登录」完成授权。";

/// ensure只允许继续使用本次仍有效的旧访问凭据，不把它当成一次成功换证。
/// 原生提前续期失败可保留未过期旧值；强制刷新则必须明确失败，不能返回旧AK冒充成功。
pub fn ensure(force: bool) -> Result<String> {
    let account = if force {
        Account::current_refresh()?
    } else {
        Account::current()?
    };
    if !force {
        account.unchanged()?;
        account.readable()?;
        return Ok(account.credential.access_key_id);
    }
    account.unchanged_refresh()?;
    Err(blocked(&account.credential, account.needs_relogin))
}

/// 仅回答原生15分钟临期策略；原Ready状态、出站和保存由宿主再次核对。
/// 缺到期值不能推断临期；非空坏时间戳按原生判定为需要换证，公开有效期仍为null。
pub fn expiring() -> Result<bool> {
    let account = Account::current_refresh()?;
    account.unchanged_refresh()?;
    Ok(!account.needs_relogin
        && account.credential.refresh_material_complete()
        && account.credential.stable_identity().is_some()
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
        "canRefresh": if material == RefreshMaterial::Complete && credential.stable_identity().is_some() && !needs_relogin { Value::Null } else { json!(false) },
        "refreshStatus": state,
        "refreshMessage": message,
        "refreshMaterialComplete": material == RefreshMaterial::Complete,
        "refreshMaterialStatus": material.name(),
        "refreshRequiresHost": material == RefreshMaterial::Complete,
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
        RefreshMaterial::Complete if credential.stable_identity().is_none() => {
            let (field, missing) = material_field([
                ("domain_id", credential.domain_id.as_str()),
                ("user_id", credential.user_id.as_str()),
            ], "domain_id / user_id");
            field_reason("identity_unconfirmed", field, missing)
        }
        RefreshMaterial::Complete => ("host_managed", gates::REFRESH.to_string()),
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
        "identity_unconfirmed" => {
            Error::http(409, message).with_code("codearts_refresh_identity_unconfirmed")
        }
        "host_managed" => Error::http(409, "CodeArts换证须由当前账号的主程序续期入口发起")
            .with_code("codearts_refresh_requires_host"),
        _ => Error::http(400, message).with_code("codearts_refresh_material_invalid"),
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
