//! Qoder PAT 交换、用户资料与设备令牌请求。错误信息不回显凭证或轮询 URL。
//!
//! ── 两家的资料接口不同 ──────────────────────────────────────
//! Qoder：`GET {openapi}/api/v1/userinfo`，正文直接就是用户对象。
//! 千问办公：`GET {gateway}/api/v1/adapter/user/account-context?include=…`，
//! 正文是 `{data:{user:{…}, plan:{…}, quota:{…}}}` —— 用户对象在
//! `data.user` 里，所以要先按站点拆一层（[`profile_view`]）。

use serde_json::{json, Value};

use agent2api_provider_protocol::WireProxy as ResolvedProxy;

use crate::errors::GatewayError;
use crate::http::send_raw;
use crate::http::ApiResponse;
use crate::logging;
use crate::proxy_util::{resolve_account_proxy, ProxyResolution};

use super::credentials::{self, Credentials};
use super::endpoints::{self, Site};

const REQUEST_TIMEOUT_MS: u64 = 15_000;

pub async fn request(
    method: &str,
    url: &str,
    body: Option<&Value>,
    headers: &[(String, String)],
    proxy: Option<&ResolvedProxy>,
) -> Result<ApiResponse, GatewayError> {
    send_raw(method, url, body, headers, proxy, Some(REQUEST_TIMEOUT_MS))
        .await
        .map_err(|error| {
            if error.is_timeout() {
                GatewayError::with_status(504, "请求超时，请检查网络后重试").with_code("qoder_transport")
            } else {
                GatewayError::with_status(502, "无法连接上游，请检查网络或账号代理设置").with_code("qoder_transport")
            }
        })
}

pub fn payload(response: ApiResponse, action: &str) -> Result<Value, GatewayError> {
    if !response.ok {
        let hint = match response.status {
            401 | 403 => "，请确认凭证有效且站点选择正确",
            429 => "，请稍后重试",
            _ => "",
        };
        return Err(GatewayError::with_status(
            i32::from(response.status),
            format!("{action}失败（HTTP {}）{hint}", response.status),
        ));
    }
    response.payload.filter(Value::is_object)
        .ok_or_else(|| GatewayError::with_status(502, format!("{action}未返回有效 JSON 对象")))
}

pub fn account_proxy(record: &Value) -> Result<Option<ResolvedProxy>, GatewayError> {
    match resolve_account_proxy(record.get("proxy")) {
        Some(ProxyResolution::Resolved(proxy)) => Ok(Some(proxy)),
        Some(ProxyResolution::Failed(reason)) => Err(GatewayError::with_status(400, reason)),
        None => Ok(None),
    }
}

/// 账号类请求的头（`Authorization` 之外的那几个）。
///
/// ── 千问办公走「最小头束」──────────────────────────────────
/// 它的账号接口（`account-context` / `wallets` / 设备轮询）是普通 REST，
/// **不做 COSY 签名**，上游看到的客户端身份来自请求头。三份参考实现
/// （dumate2api / magpie / 本项目归档）都只带 `Accept` + 自己的 `User-Agent`
/// （+ `Bearer`），没有一份带 `Cosy-*`。因此这里按最小束走 —— 带 Qoder 那套
/// 头没有依据，还可能让「头说 IDE、body 说桌面端」这类自相矛盾重新出现。
///
/// Qoder 沿用既有的固定头（它一直这么跑）。
pub fn account_headers(site: Site, token: Option<&str>) -> Vec<(String, String)> {
    if site.is_qwenwork() {
        let mut headers = vec![
            ("Accept".to_string(), "application/json".to_string()),
            (
                "User-Agent".to_string(),
                site.user_agent().unwrap_or("qoderwork/1.1.0").to_string(),
            ),
        ];
        if let Some(token) = token {
            headers.push(("Authorization".to_string(), format!("Bearer {token}")));
        }
        return headers;
    }
    endpoints::open_api_headers(token)
}

/// 把资料正文拆成「用户对象」那一层（见模块头）。
///
/// 千问办公的 `account-context` 把用户包在 `data.user` 里；拆不到就原样回，
/// 由 [`apply_profile`] 的取值链自己找。
pub fn profile_view(profile: &Value, site: Site) -> Value {
    if !site.is_qwenwork() {
        return profile.clone();
    }
    let data = profile.get("data").filter(|value| value.is_object()).unwrap_or(profile);
    match data.get("user") {
        Some(user) if user.is_object() => {
            // 计划信息在 `data.plan` 里（用户对象自己不带），并进去供名字/套餐显示
            let mut merged = user.clone();
            if let Some(plan) = data.get("plan").filter(|value| value.is_object()) {
                if let Some(object) = merged.as_object_mut() {
                    object.insert("plan".to_string(), plan.clone());
                }
            }
            merged
        }
        _ => data.clone(),
    }
}

pub async fn fetch_profile(
    token: &str,
    site: Site,
    proxy: Option<&ResolvedProxy>,
) -> Result<Value, GatewayError> {
    let response = request(
        "GET",
        &format!("{}{}", site.open_api(), site.user_info_path()),
        None,
        &account_headers(site, Some(token)),
        proxy,
    ).await?;
    payload(response, "用户信息查询")
}

pub fn apply_profile(credentials: &mut Credentials, profile: &Value) -> Result<(), GatewayError> {
    let view = profile_view(profile, credentials.site);
    let user_id = credentials::text(&view, &["id", "userId", "user_id", "uid"]);
    if !user_id.is_empty() {
        if !credentials.user_id.is_empty() && credentials.user_id != user_id {
            return Err(GatewayError::with_status(400, "凭证中的用户与账号资料不一致"));
        }
        credentials.user_id = user_id;
    }
    let name = credentials::text(&view, &["name", "username", "user_name"]);
    if !name.is_empty() {
        credentials.name = name.chars().take(100).collect();
    }
    let email = credentials::text(&view, &["email"]);
    if !email.is_empty() {
        credentials.email = email.chars().take(320).collect();
    }
    Ok(())
}

pub async fn exchange_pat(
    pat: &str,
    site: Site,
    proxy: Option<&ResolvedProxy>,
) -> Result<Credentials, GatewayError> {
    if !site.supports_pat() {
        return Err(GatewayError::with_status(
            400,
            format!("{}没有个人访问令牌（PAT），请用网页登录添加账号", site.display_name()),
        ));
    }
    let pat = credentials::secret(&json!({ "pat": pat }), &["pat"])?;
    if pat.is_empty() || pat.contains('|') {
        return Err(GatewayError::with_status(400, "请填写有效的个人访问令牌（PAT）"));
    }
    let response = request(
        "POST",
        &format!("{}{}", site.open_api(), endpoints::EXCHANGE_PATH),
        Some(&json!({ "personal_token": pat })),
        &endpoints::open_api_headers(None),
        proxy,
    ).await?;
    let data = payload(response, "PAT 换取令牌")?;
    let token = credentials::secret(&data, &["token"])?;
    let job_refresh = credentials::secret(&data, &["refresh_token"])?;
    if token.is_empty() || job_refresh.contains('|') {
        return Err(GatewayError::with_status(502, "PAT 响应缺少有效 token 或 refresh_token 格式无效"));
    }
    let profile = fetch_profile(&token, site, proxy).await?;
    let user_id = credentials::text(&profile, &["id"]);
    let machine_id = super::machine::machine_id()?;
    let mut credentials = Credentials::from_payload(&json!({
        "mode": site.id(),
        "accessToken": token,
        "refreshToken": format!("pat|{pat}|{job_refresh}|{user_id}|{machine_id}"),
        "expiresAt": credentials::timestamp(data.get("expires_at"))
            .unwrap_or_else(|| logging::now_ms() + 24 * 60 * 60 * 1000),
        "userId": user_id,
        "machineId": machine_id,
    }))?;
    credentials.set_site(site);
    apply_profile(&mut credentials, &profile)?;
    credentials.complete_identity()?;
    Ok(credentials)
}

/// 手动添加的凭证准备。`site` 来自 **provider 身份**（调用方从 kind 反查）——
/// 拆家后界面上是几张卡片，点哪张就落哪一站；payload 里的 `mode` 只是兼容
/// 字段，不再当权威（两边不一致时以 provider 为准）。
pub async fn prepare_account(payload: &Value, site: Site) -> Result<Credentials, GatewayError> {
    if payload.get("importDesktop").and_then(Value::as_bool) == Some(true) {
        return Err(GatewayError::with_status(
            400,
            format!("{}请使用网页登录添加账号", site.display_name()),
        ));
    }
    let pat = credentials::secret(payload, &["pat", "personalAccessToken", "personal_token"])?;
    if !pat.is_empty() {
        return exchange_pat(&pat, site, None).await;
    }
    let mut credentials = Credentials::from_payload(payload)?;
    credentials.set_site(site);
    if credentials.user_id.is_empty() {
        let profile = fetch_profile(&credentials.access_token, site, None).await?;
        apply_profile(&mut credentials, &profile)?;
    }
    credentials.complete_identity()?;
    Ok(credentials)
}
