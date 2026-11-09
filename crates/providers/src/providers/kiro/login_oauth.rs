//! Kiro / AWS 授权码交换。redirect 与 verifier 必须来自本轮登录，不从回填地址重建。

use serde_json::{json, Value};
use crate::http::AccountRequest;

use super::credentials::{now_ms, Credentials};
use super::{endpoints, usage};

#[derive(Clone)]
pub(super) struct AwsPending {
    pub region: String,
    pub client_id: String,
    pub client_secret: String,
    pub provider: String,
    pub state: String,
    pub verifier: String,
    pub authorize_url: String,
    pub portal_choice: Option<(String, String, String)>,
}

pub(super) fn random_token(bytes: usize) -> Result<String, String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let mut buffer = vec![0u8; bytes];
    getrandom::getrandom(&mut buffer).map_err(|_| "无法生成安全的登录凭据，请重新发起")?;
    Ok(URL_SAFE_NO_PAD.encode(buffer))
}

pub(super) fn challenge(verifier: &str) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use sha2::{Digest, Sha256};
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub(super) fn portal_url(state: &str, verifier: &str, redirect: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("state", state)
        .append_pair("code_challenge", &challenge(verifier))
        .append_pair("code_challenge_method", "S256")
        .append_pair("redirect_uri", redirect)
        .append_pair("redirect_from", "KiroIDE")
        .finish();
    format!("{}/signin?{query}", endpoints::PORTAL)
}

pub(super) async fn start_aws(
    option: &str,
    issuer: &str,
    region: &str,
    redirect: &str,
    proxy: &str,
) -> Result<AwsPending, String> {
    if !endpoints::is_aws_region(region) {
        return Err("AWS 地区不合法，请填写账号目录所在地区（例如 us-east-1）".into());
    }
    if !endpoints::is_https_url(issuer) {
        return Err("AWS 组织登录地址必须是有效的 HTTPS 地址".into());
    }
    let provider = match option {
        "builderid" => "builderid",
        "awsidc" => "enterprise",
        "internal" => "internal",
        _ => return Err("不支持的 AWS 登录方式".into()),
    };
    let state = random_token(24)?;
    let verifier = random_token(48)?;
    let body = json!({
        "clientName": "Kiro IDE", "clientType": "public", "scopes": endpoints::SCOPES,
        "grantTypes": ["authorization_code", "refresh_token"],
        "redirectUris": ["http://127.0.0.1/oauth/callback"], "issuerUrl": issuer,
    });
    let registration = post_json(
        proxy,
        &format!("{}/client/register", endpoints::oidc(region)),
        &body,
    )
    .await?;
    let client_id = text(&registration, "clientId");
    let client_secret = text(&registration, "clientSecret");
    if client_id.is_empty() || client_secret.is_empty() {
        return Err("AWS 没有返回完整的客户端注册信息".into());
    }
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", redirect)
        .append_pair("scopes", &endpoints::SCOPES.join(","))
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge(&verifier))
        .append_pair("code_challenge_method", "S256")
        .finish();
    Ok(AwsPending {
        region: region.to_string(),
        client_id,
        client_secret,
        provider: provider.into(),
        state,
        verifier,
        authorize_url: format!("{}/authorize?{query}", endpoints::oidc(region)),
        portal_choice: None,
    })
}

pub(super) async fn exchange_aws(
    aws: &AwsPending,
    code: &str,
    redirect: &str,
    proxy: &str,
) -> Result<Credentials, String> {
    let body = json!({
        "clientId": aws.client_id, "clientSecret": aws.client_secret,
        "grantType": "authorization_code", "redirectUri": redirect,
        "code": code, "codeVerifier": aws.verifier,
    });
    let token = post_json(
        proxy,
        &format!("{}/token", endpoints::oidc(&aws.region)),
        &body,
    )
    .await?;
    let access = text(&token, "accessToken");
    if access.is_empty() {
        return Err("AWS 没有返回登录凭证".into());
    }
    Ok(Credentials {
        access,
        refresh: text(&token, "refreshToken"),
        expires: expiry(&token),
        method: "idc".into(),
        login_provider: aws.provider.clone(),
        uid: aws.provider.clone(),
        region: aws.region.clone(),
        client_id: aws.client_id.clone(),
        client_secret: aws.client_secret.clone(),
        ..Default::default()
    })
}

pub(super) async fn exchange_social(
    option: &str,
    code: &str,
    verifier: &str,
    redirect: &str,
    proxy: &str,
) -> Result<Credentials, String> {
    let body = json!({ "code": code, "code_verifier": verifier, "redirect_uri": redirect });
    let token = post_json(
        proxy,
        &format!("{}/oauth/token", endpoints::AUTH_SERVICE),
        &body,
    )
    .await?;
    let access = text(&token, "accessToken");
    if access.is_empty() {
        return Err("Kiro 没有返回登录凭证".into());
    }
    Ok(Credentials {
        access,
        refresh: text(&token, "refreshToken"),
        expires: expiry(&token),
        method: "social".into(),
        login_provider: option.into(),
        uid: option.into(),
        region: "us-east-1".into(),
        profile: text(&token, "profileArn"),
        ..Default::default()
    })
}

pub(super) async fn account(mut credentials: Credentials, proxy: &str) -> Value {
    if credentials.profile.is_empty() {
        let region = credentials.region();
        if let Ok(profile) = super::refresh::lookup_profile(&credentials, &region, proxy).await {
            credentials.profile = profile;
        }
    }
    if let Ok(payload) = usage::fetch_usage_limits(&credentials, proxy).await {
        if let Some(email) = payload.pointer("/userInfo/email").and_then(Value::as_str) {
            credentials.account_id = email.to_string();
        }
        if let Some(title) = payload
            .pointer("/subscriptionInfo/subscriptionTitle")
            .and_then(Value::as_str)
        {
            credentials.plan = usage::plan_name(title);
        }
    }
    let way = match credentials.uid.as_str() {
        "google" => "Google",
        "github" => "GitHub",
        "builderid" => "Builder ID",
        "enterprise" => "IAM Identity Center",
        "internal" => "Internal",
        other => other,
    };
    let name = if credentials.account_id.is_empty() {
        "Kiro 账号".to_string()
    } else if way.is_empty() {
        credentials.account_id.clone()
    } else {
        format!("{} ({way})", credentials.account_id)
    };
    let mut secret = json!({ "name": name });
    credentials.write_into(&mut secret);
    secret
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn expiry(token: &Value) -> i64 {
    let seconds = token
        .get("expiresIn")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .unwrap_or(3600);
    now_ms().saturating_add(seconds.saturating_mul(1000))
}

async fn post_json(proxy: &str, endpoint: &str, body: &Value) -> Result<Value, String> {
    let client = crate::http::control_client(None, proxy);
    let mut response = client
        .post(endpoint)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header(
            "User-Agent",
            endpoints::ide_user_agent(&super::local::hostname()),
        )
        .timeout(std::time::Duration::from_secs(30))
        .json(body)
        .with_account_user_agent()
        .send()
        .await
        .map_err(|_| "Kiro 登录服务连接失败，请检查网络或登录代理后重新发起")?;
    let status = response.status().as_u16();
    // 上游错误正文可能带 code、clientSecret 或回调查询串，不回显或记日志。
    if !(200..300).contains(&status) {
        return Err(format!("登录被拒绝（HTTP {status}），请重新发起授权"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "读取登录服务响应失败")? {
        if bytes.len() + chunk.len() > 1024 * 1024 {
            return Err("登录服务响应过大".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| "登录返回不是有效 JSON".into())
}
