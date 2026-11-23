//! Google OAuth form 协议保留公开客户端；新流程使用 S256 PKCE，旧等待流程仍可收尾。
//! 缺 refresh_token 不落可续期账号；刷新未回新 refresh_token 时保留旧值。

use super::{client, endpoints, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};
use base64::Engine;
use sha2::{Digest, Sha256};

fn failure(status: u16, body: &Value, exchange: bool) -> Error {
    let code = body["error"].as_str().unwrap_or("");
    let (status, message) = match code {
        "invalid_grant" if exchange => (
            400,
            "Antigravity 授权码已失效、已使用或回调地址不符，请重新发起网页登录",
        ),
        "invalid_grant" => (401, "Antigravity 续期凭据已失效或被撤销，请重新授权"),
        "redirect_uri_mismatch" => (
            400,
            "Antigravity 授权回调与发起时不一致，请重新发起网页登录",
        ),
        "invalid_client" | "unauthorized_client" => (
            401,
            "Google 未接受 Antigravity 登录客户端，请重新授权或更新插件",
        ),
        _ if status == 429 => (429, "Google 登录接口请求过于频繁，请稍后重试"),
        _ => (502, "Antigravity 换取凭据失败，请检查网络或稍后重新授权"),
    };
    Error::http(status, message).with_code(if status == 401 {
        "antigravity_credentials"
    } else {
        "antigravity_oauth_failed"
    })
}

fn parse(reply: client::Reply, exchange: bool) -> Result<Value> {
    if !(200..300).contains(&reply.status) || reply.body.get("error").is_some_and(|v| !v.is_null())
    {
        return Err(failure(reply.status, &reply.body, exchange));
    }
    let access = util::header(&util::text(&reply.body, &["access_token"]), 16_384)?;
    if access.is_empty() {
        return Err(Error::http(502, "Google 换证未返回访问令牌，未覆盖账号"));
    }
    let refresh = util::header(&util::text(&reply.body, &["refresh_token"]), 16_384)?;
    if exchange && refresh.is_empty() {
        return Err(Error::http(
            400,
            "Google 未返回续期凭据，账号未保存；请在 Google 第三方访问页面撤销旧授权后重试",
        ));
    }
    let seconds = util::integer(&reply.body["expires_in"])
        .filter(|v| (1..=7 * 86400).contains(v))
        .unwrap_or(3600);
    let mut patch =
        json!({ "accessToken": access, "expiresAt": util::now()?.saturating_add(seconds * 1000) });
    if !refresh.is_empty() {
        patch["refreshToken"] = json!(refresh);
    }
    let id_token = util::header(&util::text(&reply.body, &["id_token"]), 16_384)?;
    if !id_token.is_empty() { patch["idToken"] = json!(id_token); }
    Ok(patch)
}

pub fn refresh(refresh: &str) -> Result<Value> {
    let refresh = util::header(refresh, 16_384)?;
    if refresh.is_empty() {
        return Err(Error::http(401, "Antigravity 缺少续期凭据"));
    }
    parse(
        client::form(
            endpoints::TOKEN,
            &[
                ("client_id", endpoints::CLIENT_ID),
                ("client_secret", endpoints::CLIENT_SECRET),
                ("refresh_token", &refresh),
                ("grant_type", "refresh_token"),
            ],
            &endpoints::oauth_ua(),
        )?,
        false,
    )
}

pub fn exchange(code: &str, redirect: &str, verifier: Option<&str>) -> Result<Value> {
    if code.is_empty() || code.len() > 8192 || code.chars().any(char::is_control) {
        return Err(Error::http(400, "Antigravity 授权码格式无效"));
    }
    let mut fields = vec![
        ("client_id", endpoints::CLIENT_ID),
        ("client_secret", endpoints::CLIENT_SECRET),
        ("code", code),
        ("redirect_uri", redirect),
        ("grant_type", "authorization_code"),
    ];
    if let Some(verifier) = verifier {
        verifier_value(verifier)?;
        fields.push(("code_verifier", verifier));
    }
    parse(
        client::form(endpoints::TOKEN, &fields, &endpoints::oauth_ua())?,
        true,
    )
}

pub fn email(token: &str, check: impl Fn() -> Result<()>) -> Result<Option<String>> {
    let token = util::header(token, 16_384)?;
    for url in endpoints::USERINFO {
        check()?;
        let result = client::get(
            url,
            json!({ "Accept": "application/json", "User-Agent": endpoints::oauth_ua(),
            "Authorization": format!("Bearer {token}") }),
        );
        check()?;
        if let Ok(body) = result.and_then(client::Reply::accepted) {
            let email = util::text(&body, &["email"]);
            if !email.is_empty() {
                if let Ok(email) = util::header(&email, 480) {
                    return Ok(Some(email));
                }
            }
        }
    }
    Ok(None)
}

fn verifier_value(verifier: &str) -> Result<()> {
    if !(43..=128).contains(&verifier.len())
        || !verifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'))
    {
        return Err(Error::http(
            409,
            "Antigravity 授权校验材料已失效，请重新发起",
        ));
    }
    Ok(())
}

pub fn authorize(redirect: &str, state: &str, verifier: &str) -> Result<String> {
    verifier_value(verifier)?;
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let mut url =
        url::Url::parse(endpoints::AUTH).map_err(|_| Error::new("Google 授权地址无效"))?;
    url.query_pairs_mut()
        .append_pair("client_id", endpoints::CLIENT_ID)
        .append_pair("redirect_uri", redirect)
        .append_pair("response_type", "code")
        .append_pair("scope", &endpoints::SCOPES.join(" "))
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .append_pair("include_granted_scopes", "true")
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    Ok(url.to_string())
}
