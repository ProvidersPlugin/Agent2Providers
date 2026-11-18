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
const NO_REFRESH: &str = "CodeArts 当前临时凭据没有 refresh_token，不能自动换证；原生 ticket 通道也不提供刷新令牌，到期后需要重新登录，未重放登录票据";

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

fn reason(credential: &Credential, needs_relogin: bool) -> (&'static str, &'static str) {
    if needs_relogin {
        return ("authorization_expired", RELOGIN);
    }
    match credential.refresh_material() {
        RefreshMaterial::MissingToken => ("requires_relogin", NO_REFRESH),
        RefreshMaterial::MissingContext => ("missing_oauth_context",
            "CodeArts 刷新令牌必须配合同次授权的 OAuth 上下文；不能新建PKCE或DPoP材料替代，请完整导入"),
        RefreshMaterial::InvalidClient => ("invalid_client_id",
            "CodeArts 原授权客户端标识无效，不能改用另一个客户端换证，请完整导入原授权材料"),
        RefreshMaterial::InvalidVerifier => ("invalid_pkce_verifier",
            "CodeArts 缺少合法的原PKCE verifier，不能只靠刷新令牌换证，请完整导入原授权材料"),
        RefreshMaterial::InvalidProofKey => ("invalid_dpop_key",
            "CodeArts 原DPoP私钥缺失、无效或与公钥不一致，未生成新密钥替代"),
        RefreshMaterial::Complete if credential.stable_identity().is_none() => (
            "identity_unconfirmed", gates::IDENTITY),
        RefreshMaterial::Complete => ("host_managed", gates::REFRESH),
    }
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
