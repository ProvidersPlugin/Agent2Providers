//! STS 两种 grant 共用同一 DPoP 私钥。续期令牌一次性轮换，响应缺新令牌绝不报成功。
//! 2小时仅沿用旧控制面到期兜底；模型 Basic 凭据不使用这个时间。

use super::{client, dpop::DpopKey, endpoints, errors, identity_claims, util};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

fn request(pair: &Value, fields: &[(&str, &str)], refresh: bool) -> Result<Value> {
    let proof = DpopKey::from_json(pair)?.proof("POST", endpoints::TOKEN)?;
    let form = fields
        .iter()
        .map(|(key, value)| format!("{}={}", util::encode(key), util::encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let response = client::open_raw("POST", endpoints::TOKEN,
        json!({ "DPoP": proof, "Content-Type": "application/x-www-form-urlencoded", "Accept": "application/json" }), Some(form.as_bytes()))
        .map_err(|error| transport(error, refresh))?;
    let reply = client::read(response, false).map_err(|error| transport(error, refresh))?;
    if !(200..300).contains(&reply.status)
        || reply
            .value
            .get("error")
            .is_some_and(|value| !value.is_null())
    {
        let detail = util::text(&reply.value, &["error_description", "error_msg", "error"]);
        let secrets: Vec<&str> = fields.iter().map(|(_, value)| *value).collect();
        let detail = errors::clean(&detail, &secrets);
        let hint = if detail.contains("refresh token has been used") || detail.contains("STS5.1806")
        {
            "；该刷新令牌已被其他客户端使用，请重新网页登录取得独立授权，不要继续复用旧导入凭据"
        } else {
            ""
        };
        let mut error = Error::http(
            if refresh { 401 } else { 400 },
            format!(
                "OfficeAce {}被拒（HTTP {}）：{detail}{hint}",
                if refresh { "续期" } else { "授权换码" },
                reply.status
            ),
        )
        .with_code("officeace_authorization_rejected");
        errors::redact(&mut error, pair);
        error.credential_scope = Some("oauth".into());
        return Err(if refresh && (reply.status >= 500 || reply.status == 408) {
            transport(error, true)
        } else {
            error
        });
    }
    let temporary = &reply.value["credentials"];
    let ak = util::text(temporary, &["access_key_id"]);
    let sk = util::text(temporary, &["secret_access_key"]);
    let security = util::text(temporary, &["security_token"]);
    let next = util::text(&reply.value, &["refresh_token"]);
    if ak.is_empty() || sk.is_empty() || security.is_empty() || refresh && next.is_empty() {
        return Err(transport(
            Error::http(
                502,
                "OfficeAce 令牌响应缺少完整临时凭据或轮换后的刷新令牌，请重新登录",
            ),
            refresh,
        ));
    }
    let expiration = util::parse_ms(&temporary["expiration"]);
    let identity = identity_claims::response(&reply.value);
    Ok(
        json!({ "accessKeyId": ak, "secretAccessKey": sk, "securityToken": security,
        "projectId": util::text(temporary, &["project_id"]),
        "expiresAt": if expiration > 0 { expiration } else { sdk::now().saturating_add(2 * 3600 * 1000) },
        "expiresAtEstimated": expiration <= 0,
        "refreshToken": next, "accountName": identity.name, "principalId": identity.principal }),
    )
}

fn transport(mut error: Error, refresh: bool) -> Error {
    if refresh && !errors::permission(&error) {
        error.code = Some(errors::REFRESH_UNCERTAIN.into());
        error.message = "OfficeAce 一次性续期结果尚未确认，未自动重放刷新令牌；模型对话凭据未被改动，请重新登录授权".into();
    }
    error.credential_scope = Some("oauth".into());
    error
}

pub fn exchange(code: &str, verifier: &str, pair: &Value) -> Result<Value> {
    request(
        pair,
        &[
            ("client_id", endpoints::CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("grant_type", "authorization_code"),
            ("redirect_uri", endpoints::CALLBACK),
        ],
        false,
    )
}

pub fn refresh(pair: &Value, token: &str) -> Result<Value> {
    request(
        pair,
        &[
            ("client_id", endpoints::CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", token),
        ],
        true,
    )
}
