use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::Value;

use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;
use crate::logging;

pub(super) const EXPIRED_CODE: &str = "refresh_token_expired";
const EXPIRED_MESSAGE: &str = "CodeArts 登录授权已到期，无法继续自动续期，请重新完成网页登录。自动续期只更新临时访问凭据，不会延长本次登录授权的截止时间。";

pub(super) fn token_error(status: u16, payload: &Value) -> Option<GatewayError> {
    if status != 400 || payload.get("error_code").and_then(Value::as_str) != Some("STS5.1806") {
        return None;
    }
    let message = payload.get("error_msg").and_then(Value::as_str)?.trim();
    // STS5.1806 也用于令牌解析失败或重复使用，不能仅凭业务码永久停止续期。
    if !message.eq_ignore_ascii_case("invalid refresh token: 'refresh code has expired'") {
        return None;
    }
    Some(GatewayError::with_status(400, EXPIRED_MESSAGE).with_code(EXPIRED_CODE))
}

pub(super) fn relogin_error(record: &Value) -> Option<GatewayError> {
    if record.get("needsRelogin").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let reason = record.get("needsReloginReason").and_then(Value::as_str)
        .filter(|reason| !reason.trim().is_empty()).unwrap_or(EXPIRED_MESSAGE);
    Some(GatewayError::with_status(400, reason).with_code(EXPIRED_CODE))
}

pub(super) fn expires_at_ms(refresh_token: &str) -> Option<i64> {
    // JWT 的 exp 只用于展示，永久停止续期必须以上游的明确拒绝为准。
    let payload = refresh_token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    let expiry = claims.get("exp")?.as_i64()?;
    expiry.checked_mul(1000).filter(|expiry| *expiry > 0)
}

pub(super) async fn remember_expired(
    ctx: &PluginCtx,
    account_id: &str,
    record: &Value,
    error: &GatewayError,
) {
    let expect = crate::store::secret_view(record);
    let next = crate::store::with_fields(&expect, &[
        ("needsRelogin", Value::Bool(true)),
        ("needsReloginReason", Value::String(error.message.clone())),
    ]);
    match crate::store::save(ctx, account_id, &expect, &next).await {
        Ok(true) => logging::log(
            "[CodeArts]",
            &format!("账号 {account_id} 的登录授权已到期，已标记为需要重新登录并停止自动续期"),
        ),
        Ok(false) => {}
        Err(failure) => logging::log(
            "[CodeArts]",
            &format!("账号 {account_id} 的登录授权已到期，但重新登录状态未能保存：{}", failure.message),
        ),
    }
}
