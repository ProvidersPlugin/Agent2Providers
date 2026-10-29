//! OfficeAce 自助 OAuth：**不需要桌面端**，全链路在插件这一侧跑完。
//!
//! ── 九步（依据：本机 OfficeAce 的 `green-package/api/dist/auth/huawei-oauth.js`
//! 与 `officeace2api` 的 `oauth.mjs`，两处逐条一致）───────────────
//! ```text
//! ① POST {cloud}/v1/claw/auth/state                     → state（无鉴权）
//! ② 本地生成 PKCE(S256) 与一对 P-256 DPoP 密钥
//! ③ 拼 authorizeUrl，让用户去浏览器登华为云
//! ④ GET  {cloud}/v1/claw/auth/code?state=<state>        ← 整件事的钥匙
//!      202 = 还没登完；404 = state 不认识/过期；200 = 拿到 authorization code
//! ⑤ POST {sts}/v1/oauth2/tokens（表单 + `DPoP:` 头）    → 临时 AK/SK/STS
//!      （响应**不带 project_id**，IAM OAuth2 的既成事实）
//! ⑥ GET  {cloud}/v1/claw/client-permission-validate（SDK-HMAC 签名）
//!      → 身份三件套 + `subscription.model_info`（**可能为空** = 尚未开通）
//! ⑥a GET  {iam}/v3/projects（SDK-HMAC 签名，仅 project_id 空时）
//!      → 按 region 名匹配补 project_id（官方 `resolveProjectId` 同口径）
//! ⑥b POST {cloud}/v1/claw/client-subscription（body `{}`，仅 model_info 空时）
//!      → 把服务开通，成功返回顶层 `model_info`（官方对「未开通」的处置）
//! ⑦ 入库：网关 Basic 那对**不过期**，临时凭据用来续期与额度/签到
//! ```
//!
//! ── 为什么必须轮询（而不是等回调）────────────────────────────
//! 云端回调页（`/v1/claw/auth/callback`）只是终点页，**不往本机跳** ——
//! 桌面端就是靠轮询 ④ 把授权码取回来的。所以本家报 `poll_interval_ms > 0`
//! （网关起后台任务问 `login.poll`），不注册任何回调路由。
//!
//! ── 轮次状态怎么保存 ────────────────────────────────────────
//! 进程内一张表（`flow` → `Pending`）。它只在一次登录的几分钟里有用，插件重启
//! 就丢 —— 那是正确行为（用户重新点一次即可），不值得落盘。
//!
//! ── 续期也在这里（不再留在网关）─────────────────────────────
//! 续期要用**登录时那把 DPoP 私钥**签一份新 proof 去打同一个令牌端点。私钥
//! 与令牌都在账号 secret 里，插件读得到；把它留在网关只会让「上游协议细节」
//! 继续留在主仓库 —— 而这正是这次拆分要消掉的东西。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 本 crate 编译期禁止 unwrap/expect/panic（见 `main.rs` 的 deny）。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};

use crate::dpop::DpopKey;
use crate::errors::GatewayError;
use crate::http::describe_chain;
use crate::logging;

use super::signer::{self, Algorithm, Credential as SigningCredential};

/// 云端总控基址（`/v1/claw/*` 都挂在这里）。
pub const CLOUD_BASE: &str = "https://agentarts.cn-southwest-2.myhuaweicloud.com";
/// 华为云登录页基址。
pub const AUTH_BASE: &str = "https://auth.huaweicloud.com";
/// 固定的 client_id（官方实现同款）。
pub const CLIENT_ID: &str = "pdp5_for_agentarts";
/// 授权回调**指向云端**（不是本机 loopback）。
pub const REDIRECT_URI: &str =
    "https://agentarts.cn-southwest-2.myhuaweicloud.com/v1/claw/auth/callback";
/// 令牌端点（与 CodeArts 同一个 STS 主机）。
pub const TOKEN_URL: &str = "https://sts.cn-north-4.myhuaweicloud.com/v1/oauth2/tokens";
/// 区域（控制面签名用）。
pub const REGION: &str = "cn-southwest-2";
/// IAM keystone 基址（官方 `DEFAULT_IAM_KEYSTONE_BASE`）。
const IAM_KEYSTONE_BASE: &str = "https://iam.myhuaweicloud.com";
/// 控制面订阅族的通行头（官方 `checkPermission` / `subscriptionClaw` 都带）。
pub const SUBSCRIPTION_TYPE_HEADER: (&str, &str) = ("x-subscription-type", "v2");

/// 轮询间隔（官方实现 2.5 秒）。
const POLL_INTERVAL_MS: u64 = 2_500;
/// 单次请求超时。
const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 一轮登录在进程内的状态。
struct Pending {
    code_verifier: String,
    dpop_key_pair: Value,
    /// 上一次轮询取码失败的原因（`404` 这类作废情形要立刻报出来，不能拖到超时）
    failure: Option<String>,
}

fn flows() -> &'static Mutex<HashMap<String, Pending>> {
    static FLOWS: OnceLock<Mutex<HashMap<String, Pending>>> = OnceLock::new();
    FLOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_flows<T>(action: impl FnOnce(&mut HashMap<String, Pending>) -> T) -> T {
    match flows().lock() {
        Ok(mut table) => action(&mut table),
        Err(poisoned) => action(&mut poisoned.into_inner()),
    }
}

/// `encodeURIComponent` 口径的百分号编码（PKCE challenge 与 URL 参数都用它）。
pub fn url_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.' | b'~') {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn base64url(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    URL_SAFE_NO_PAD.encode(bytes)
}

fn random_bytes(count: usize) -> Vec<u8> {
    let mut buffer = vec![0u8; count];
    if getrandom::getrandom(&mut buffer).is_err() {
        // 取不到随机源时退化为时间戳派生（state/verifier 仍在本机唯一）
        let now = logging::now_ms();
        for (index, slot) in buffer.iter_mut().enumerate() {
            *slot = ((now >> (index % 8 * 8)) & 0xff) as u8;
        }
    }
    buffer
}

/// 第一步：向云端要一个 state（**无鉴权**）。
async fn fetch_state() -> Result<String, String> {
    let url = format!("{CLOUD_BASE}/v1/claw/auth/state");
    let response = crate::http::control_client(None, "direct")
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .body("{}")
        .send()
        .await
        .map_err(|error| format!("向云端申请 state 失败：{}", describe_chain(&error)))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let state = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|payload| payload.get("state").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    if state.trim().is_empty() {
        return Err(format!("云端没给出 state（HTTP {status}）"));
    }
    Ok(state.trim().to_string())
}

/// 第三步：拼授权地址（官方写法：外层 `login.html?service=<内层 authorize>`）。
fn build_authorize_url(state: &str, challenge: &str) -> String {
    let inner = format!(
        "{AUTH_BASE}/authui/v1/oauth2/authorize?client_id={CLIENT_ID}\
         &code_challenge={}&code_challenge_method=SHA-256&state={}\
         &scope=openid&redirect_uri={}&response_type=code",
        url_escape(challenge),
        url_escape(state),
        url_escape(REDIRECT_URI),
    );
    format!("{AUTH_BASE}/authui/login.html?service={}", url_escape(&inner))
}

/// 发起一轮登录（①②③），返回 `(flow, auth_url, poll_interval_ms)`。
pub async fn start() -> Result<(String, String, u64), String> {
    let state = fetch_state().await?;
    // PKCE：verifier 是 43–128 位随机串；challenge = base64url(sha256(verifier))
    let code_verifier = base64url(&random_bytes(32));
    let challenge = {
        use sha2::{Digest, Sha256};
        base64url(&Sha256::digest(code_verifier.as_bytes()))
    };
    let dpop_key_pair = DpopKey::generate().map_err(|error| format!("生成 DPoP 密钥失败：{error}"))?;
    let auth_url = build_authorize_url(&state, &challenge);
    with_flows(|table| {
        table.insert(
            state.clone(),
            Pending {
                code_verifier,
                dpop_key_pair,
                failure: None,
            },
        );
    });
    Ok((state, auth_url, POLL_INTERVAL_MS))
}

/// 取消一轮登录（把待办删掉，之后的 poll 报「不存在」）。
pub fn cancel(flow: &str) {
    with_flows(|table| {
        table.remove(flow);
    });
}

/// 单次取码。`Ok(None)` = 还没登完（202）；`Err` = 作废（404）或其它失败。
async fn poll_code(state: &str) -> Result<Option<String>, String> {
    let url = format!("{CLOUD_BASE}/v1/claw/auth/code?state={}", url_escape(state));
    let response = crate::http::control_client(None, "direct")
        .get(&url)
        .header("Accept", "application/json")
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .send()
        .await
        .map_err(|error| format!("轮询授权码失败：{}", describe_chain(&error)))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    match status {
        // 用户还没在浏览器里点完 —— 正常，继续等
        202 => Ok(None),
        // state 不认识或过期：**立即判作废**，别拖到超时
        404 => Err("授权码已失效或 state 不认识，请重新发起登录".to_string()),
        code_status if !(200..300).contains(&code_status) => {
            Err(format!("取授权码返回 HTTP {code_status}"))
        }
        _ => {
            let payload = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
            if let Some(error) = payload.get("error").and_then(Value::as_str) {
                let message = payload
                    .get("error_msg")
                    .and_then(Value::as_str)
                    .unwrap_or(error);
                return Err(format!("云端拒绝：{message}"));
            }
            let code = payload
                .get("code")
                .and_then(Value::as_str)
                .or_else(|| payload.get("authorization_code").and_then(Value::as_str))
                .map(str::trim)
                .unwrap_or("")
                .to_string();
            if code.is_empty() {
                return Err("云端说好了，但没给授权码".to_string());
            }
            Ok(Some(code))
        }
    }
}

/// 轮询一次：`Ok(None)` = 还没好；`Ok(Some(secret))` = 完成（返回落库形态的 secret）。
pub async fn poll(flow: &str) -> Result<Option<Value>, GatewayError> {
    let (code_verifier, dpop_key_pair, failure) = match with_flows(|table| {
        table.get(flow).map(|pending| {
            (
                pending.code_verifier.clone(),
                pending.dpop_key_pair.clone(),
                pending.failure.clone(),
            )
        })
    }) {
        Some(values) => values,
        None => {
            return Err(GatewayError::with_status(
                404,
                "登录任务不存在或已过期，请重新发起",
            ))
        }
    };
    if let Some(failure) = failure {
        with_flows(|table| {
            table.remove(flow);
        });
        return Err(GatewayError::with_status(400, failure));
    }
    let code = match poll_code(flow).await {
        Ok(Some(code)) => code,
        Ok(None) => return Ok(None),
        Err(error) => {
            with_flows(|table| {
                table.remove(flow);
            });
            return Err(GatewayError::with_status(400, error));
        }
    };
    let credential = finish(&code, &code_verifier, &dpop_key_pair).await;
    with_flows(|table| {
        table.remove(flow);
    });
    match credential {
        Ok(credential) => Ok(Some(secret_of(&credential))),
        Err(error) => Err(GatewayError::with_status(400, error)),
    }
}

/// 登录成功后的凭据（⑦ 的产物）：两层都在。
pub struct GatewayCredential {
    pub base_url: String,
    pub model_app_key: String,
    pub model_app_secret: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub security_token: String,
    pub project_id: String,
    pub expires_at: i64,
    /// 上游给的账号显示名（取不到为空串）。
    pub user_name: String,
    /// 上游账号 id（`client-permission-validate` 的 `account_id`）。
    pub account_id: String,
    /// IAM **用户级** id（比 `account_id` 更细）。
    pub principal_id: String,
    /// 一次性 refresh token（换新的临时 AK/SK 用；上游每轮轮换）。
    pub refresh_token: String,
    /// 这次登录用的 DPoP 密钥对（续期签 proof 要用**同一把**私钥）。
    pub dpop_key_pair: Value,
}

/// 落库形态的 secret（与网关那版逐字同形 —— 存量记录不能改形状）。
fn secret_of(credential: &GatewayCredential) -> Value {
    let mut secret = json!({
        "baseUrl": credential.base_url,
        "modelAppKey": credential.model_app_key,
        "modelAppSecret": credential.model_app_secret,
        "accessKeyId": credential.access_key_id,
        "secretAccessKey": credential.secret_access_key,
        "securityToken": credential.security_token,
        "projectId": credential.project_id,
        "expiresAt": credential.expires_at,
        "refreshToken": credential.refresh_token,
        "dpopKeyPair": credential.dpop_key_pair,
    });
    if let Some(object) = secret.as_object_mut() {
        for (key, value) in [
            ("accountName", &credential.user_name),
            ("accountId", &credential.account_id),
            ("principalId", &credential.principal_id),
        ] {
            if !value.trim().is_empty() {
                object.insert(key.to_string(), Value::String(value.clone()));
            }
        }
    }
    secret
}

/// ⑤⑥⑦：拿授权码换临时凭据，再用它换网关 Basic 那对。
async fn finish(
    code: &str,
    code_verifier: &str,
    dpop_key_pair: &Value,
) -> Result<GatewayCredential, String> {
    let (mut temporary, expires_at, identity, refresh_token) =
        exchange_code(code, code_verifier, dpop_key_pair).await?;
    // ① project_id 补查
    if temporary.project_id.is_empty() {
        if let Some(project_id) = resolve_project_id(&temporary, REGION).await {
            temporary.project_id = project_id;
        }
    }
    // ② 权限校验：identity +（可能为空的）model_info
    let (account_id, permission_principal, model_info) = fetch_permission(&temporary).await?;
    // ③ 没开通就开通一次 —— 官方对「model_info 为空」的处置就是这条路
    let model_info = if model_info.is_object() {
        model_info
    } else {
        logging::log(
            "[OfficeAce]",
            "账号尚未开通，尝试自动开通（client-subscription）",
        );
        subscribe_model_info(CLOUD_BASE, &temporary, None).await?
    };
    let (base_url, model_app_key, model_app_secret) = gateway_of(&model_info)?;
    let principal_id = if identity.principal_id.is_empty() {
        permission_principal
    } else {
        identity.principal_id
    };
    Ok(GatewayCredential {
        base_url,
        model_app_key,
        model_app_secret,
        account_id,
        principal_id,
        access_key_id: temporary.access_key_id,
        secret_access_key: temporary.secret_access_key,
        security_token: temporary.security_token,
        project_id: temporary.project_id,
        expires_at,
        user_name: identity.user_name,
        refresh_token,
        dpop_key_pair: dpop_key_pair.clone(),
    })
}

/// 用一次性 refresh_token 换一套新的临时 AK/SK（面板「刷新 Token」与自动维护走这条）。
///
/// 上游令牌端点两个 grant 共用；`refresh_token` **一次性轮换**，所以返回值里那份
/// 新的必须落盘（旧的重放会得到 `STS5.1806 the refresh token has been used`）。
///
/// `dpop_key_pair` 必须是**当初登录那把**（proof 的 `jwk` 头要与授权时一致）。
/// 返回 `(新临时凭据, 到期毫秒, 显示名, 轮换后的 refresh_token)`。
pub async fn refresh(
    dpop_key_pair: &Value,
    refresh_token: &str,
) -> Result<(SigningCredential, i64, String, String), String> {
    if refresh_token.trim().is_empty() {
        return Err("OfficeAce 账号没有 refresh token，只能重新登录授权".to_string());
    }
    let key = DpopKey::from_json(dpop_key_pair)
        .map_err(|error| format!("DPoP 私钥不可用：{error}"))?;
    let proof = key
        .proof("POST", TOKEN_URL, logging::now_ms())
        .map_err(|error| format!("生成 DPoP proof 失败：{error}"))?;
    let form = [
        ("client_id", CLIENT_ID),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token.trim()),
    ]
    .iter()
    .map(|(name, value)| format!("{}={}", url_escape(name), url_escape(value)))
    .collect::<Vec<_>>()
    .join("&");
    let response = crate::http::control_client(None, "direct")
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .header("DPoP", proof)
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .body(form)
        .send()
        .await
        .map_err(|error| format!("续期请求失败：{}", describe_chain(&error)))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let payload = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        // 错误体里可能回显发过去的东西（表单里躺着 refresh_token）—— 只取 code/message
        let detail = payload
            .get("error_description")
            .and_then(Value::as_str)
            .or_else(|| payload.get("error_msg").and_then(Value::as_str))
            .or_else(|| payload.get("error").and_then(Value::as_str))
            .unwrap_or("");
        return Err(format!("续期被拒（HTTP {status}）：{detail}"));
    }
    let credentials = payload.get("credentials").unwrap_or(&Value::Null);
    let access_key_id = text_in(credentials, "access_key_id");
    let secret_access_key = text_in(credentials, "secret_access_key");
    if access_key_id.is_empty() || secret_access_key.is_empty() {
        return Err("续期响应没有新的 AK/SK".to_string());
    }
    // 显示名也从这一轮的重发里取（参考实现同源：每次续期都解一遍 claims）
    let user_name = response_identity(&payload).user_name;
    let expires_at = credentials
        .get("expiration")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_ms)
        .filter(|value| *value > 0)
        .unwrap_or_else(|| logging::now_ms() + 2 * 3600 * 1000);
    let next_refresh = payload
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "续期响应缺少新的 refresh_token，旧令牌可能已被消费，请重新登录授权".to_string())?
        .to_string();
    Ok((
        SigningCredential {
            access_key_id,
            secret_access_key,
            security_token: text_in(credentials, "security_token"),
            project_id: text_in(credentials, "project_id"),
        },
        expires_at,
        user_name,
        next_refresh,
    ))
}

/// ⑤ 授权码 → 临时 AK/SK（表单 + `DPoP:` 头）。
async fn exchange_code(
    code: &str,
    code_verifier: &str,
    dpop_key_pair: &Value,
) -> Result<(SigningCredential, i64, TokenIdentity, String), String> {
    let key = DpopKey::from_json(dpop_key_pair)
        .map_err(|error| format!("DPoP 私钥不可用：{error}"))?;
    let proof = key
        .proof("POST", TOKEN_URL, logging::now_ms())
        .map_err(|error| format!("生成 DPoP proof 失败：{error}"))?;
    let form = [
        ("client_id", CLIENT_ID),
        ("code", code.trim()),
        ("code_verifier", code_verifier),
        ("grant_type", "authorization_code"),
        ("redirect_uri", REDIRECT_URI),
    ]
    .iter()
    .map(|(name, value)| format!("{}={}", url_escape(name), url_escape(value)))
    .collect::<Vec<_>>()
    .join("&");
    let response = crate::http::control_client(None, "direct")
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .header("DPoP", proof)
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .body(form)
        .send()
        .await
        .map_err(|error| format!("换取临时凭据失败：{}", describe_chain(&error)))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(format!("令牌端点被拒（HTTP {status}）"));
    }
    let payload = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
    let credentials = payload.get("credentials").unwrap_or(&Value::Null);
    let access_key_id = text_in(credentials, "access_key_id");
    let secret_access_key = text_in(credentials, "secret_access_key");
    if access_key_id.is_empty() || secret_access_key.is_empty() {
        return Err(format!("令牌端点没返回临时凭据（HTTP {status}）"));
    }
    // 到期时刻：优先上游给的 `expiration`，缺省按「现在 + 2 小时」估算（实测值）
    let expires_at = credentials
        .get("expiration")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_ms)
        .filter(|value| *value > 0)
        .unwrap_or_else(|| logging::now_ms() + 2 * 3600 * 1000);
    let identity = response_identity(&payload);
    let refresh_token = payload
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok((
        SigningCredential {
            access_key_id,
            secret_access_key,
            security_token: text_in(credentials, "security_token"),
            project_id: text_in(credentials, "project_id"),
        },
        expires_at,
        identity,
        refresh_token,
    ))
}

/// ⑥ 用临时凭据做一次权限校验（SDK-HMAC-SHA256 签名）。
///
/// 返回 `(account_id, principal_id, model_info)`：身份两项只用来给账号名兜底；
/// `model_info` **可能为空对象**——那是「账号尚未开通」的正常形态。
async fn fetch_permission(
    temporary: &SigningCredential,
) -> Result<(String, String, Value), String> {
    let url = format!("{CLOUD_BASE}/v1/claw/client-permission-validate");
    // 头对齐官方 `checkPermission`：订阅类型头 +（signer 自动补的）
    // `X-Security-Token` / `X-Project-ID`。GET 无 body、也就**不带** `Content-Type`。
    let headers = signer::sign(&signer::SignRequest {
        algorithm: Algorithm::Sdk,
        method: "GET",
        url: &url,
        headers: &[(
            SUBSCRIPTION_TYPE_HEADER.0.to_string(),
            SUBSCRIPTION_TYPE_HEADER.1.to_string(),
        )],
        body: b"",
        credential: temporary,
        region: REGION,
        date_override: None,
    })?;
    let mut request = crate::http::control_client(None, "direct")
        .get(&url)
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS));
    for (name, value) in &headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("权限校验失败：{}", describe_chain(&error)))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let payload = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let detail = payload
            .get("error_msg")
            .and_then(Value::as_str)
            .or_else(|| payload.get("error_code").and_then(Value::as_str))
            .unwrap_or("");
        return Err(format!("权限校验失败（HTTP {status}）：{detail}"));
    }
    // `model_info` 的两种位置（实测）：validate 嵌在 `subscription.model_info`；
    // 开通那一发在顶层。两层都兜，非对象一律当「没有」。
    let model_info = payload
        .get("model_info")
        .or_else(|| payload.get("subscription").and_then(|value| value.get("model_info")))
        .or_else(|| payload.get("modelInfo"))
        .filter(|value| value.is_object())
        .cloned()
        .unwrap_or(Value::Null);
    let account_id = text_in(&payload, "account_id");
    let principal_id = text_in(&payload, "principal_id");
    Ok((account_id, principal_id, model_info))
}

/// ⑥a 补查 `project_id`：`GET {iam}/v3/projects` 里按 region 名匹配启用中的项目。
///
/// 返回 `None` = 没查到 —— 调用方如实把空串留给下游，并且**不**把它当登录失败
/// （validate 那一发不缺它也过）。
async fn resolve_project_id(credential: &SigningCredential, region: &str) -> Option<String> {
    let url = format!("{IAM_KEYSTONE_BASE}/v3/projects");
    let headers = signer::sign(&signer::SignRequest {
        algorithm: Algorithm::Sdk,
        method: "GET",
        url: &url,
        headers: &[],
        body: b"",
        credential,
        region,
        date_override: None,
    })
    .ok()?;
    let mut request = crate::http::control_client(None, "direct")
        .get(&url)
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS));
    for (name, value) in &headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request.send().await.ok()?;
    if !(200..300).contains(&response.status().as_u16()) {
        return None;
    }
    let payload = response.json::<Value>().await.ok()?;
    let projects = payload.get("projects").and_then(Value::as_array)?;
    projects.iter().find_map(|project| {
        let name = project.get("name").and_then(Value::as_str)?;
        let id = project.get("id").and_then(Value::as_str)?;
        let enabled = project
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        (name == region && enabled && !id.is_empty()).then(|| id.to_string())
    })
}

/// ⑥b 开通：`POST {claw}/v1/claw/client-subscription`，成功返回**顶层** `model_info`。
///
/// `promotion_code` 为 `None` 时 body 发 `{}`（官方首次开通就是这个形状）；
/// 失败时返回面向用户的中文错误（邀请码族错误码单独给准确提示）。
async fn subscribe_model_info(
    cloud_base: &str,
    credential: &SigningCredential,
    promotion_code: Option<&str>,
) -> Result<Value, String> {
    let url = format!("{cloud_base}/v1/claw/client-subscription");
    let body = match promotion_code.map(str::trim).filter(|code| !code.is_empty()) {
        Some(code) => json!({ "promotion_code": code }).to_string(),
        None => "{}".to_string(),
    };
    let headers = signer::sign(&signer::SignRequest {
        algorithm: Algorithm::Sdk,
        method: "POST",
        url: &url,
        headers: &[
            (
                "Content-Type".to_string(),
                "application/json;charset=utf8".to_string(),
            ),
            (
                SUBSCRIPTION_TYPE_HEADER.0.to_string(),
                SUBSCRIPTION_TYPE_HEADER.1.to_string(),
            ),
        ],
        body: body.as_bytes(),
        credential,
        region: REGION,
        date_override: None,
    })?;
    let mut request = crate::http::control_client(None, "direct")
        .post(&url)
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .body(body);
    for (name, value) in &headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("开通请求失败：{}", describe_chain(&error)))?;
    let status = response.status().as_u16();
    let payload = response.json::<Value>().await.unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let error_code = payload
            .get("error_code")
            .and_then(Value::as_str)
            .unwrap_or("");
        let detail = payload
            .get("error_msg")
            .and_then(Value::as_str)
            .or_else(|| payload.get("error_message").and_then(Value::as_str))
            .unwrap_or("");
        if let Some(hint) = promotion_hint(error_code) {
            return Err(format!(
                "该账号尚未开通果办服务（{hint}）：请先在 OfficeAce 客户端完成开通，再回来重新登录"
            ));
        }
        let shown = if detail.is_empty() { error_code } else { detail };
        return Err(format!("开通失败（HTTP {status}）：{shown}"));
    }
    payload
        .get("model_info")
        .filter(|value| value.is_object())
        .cloned()
        .ok_or_else(|| "开通成功但云端仍未返回模型网关信息，请稍后重试".to_string())
}

/// 邀请码族错误码 → 给用户的中文提示（官方 `PROMOTION_CODE_ERROR_MESSAGES` 的中文口径）。
fn promotion_hint(error_code: &str) -> Option<&'static str> {
    match error_code {
        "AgentArts.11000008" => Some("邀请码无效"),
        "AgentArts.11000009" => Some("邀请码不存在或今日配额已用完"),
        "AgentArts.00000104" | "common.01010003" => Some("当前账号权限不足"),
        "common.01010004" => Some("账号可能未实名认证或处于欠费状态"),
        _ => None,
    }
}

/// 从 `model_info` 解网关三件套：基址（规范化到 `/v2`）+ app key/secret。
fn gateway_of(model_info: &Value) -> Result<(String, String, String), String> {
    let auth_info = model_info.get("model_auth_info").unwrap_or(&Value::Null);
    let app_key = text_in(auth_info, "model_app_key");
    let app_secret = text_in(auth_info, "model_app_secret");
    if app_key.is_empty() || app_secret.is_empty() {
        return Err(
            "云端返回了模型信息，但缺少模型网关凭据（model_app_key / model_app_secret）".to_string(),
        );
    }
    let host = model_info
        .get("model_api_url_base")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    let base_url = if host.is_empty() {
        String::new()
    } else if host.ends_with("/v2") {
        format!("https://{host}")
    } else {
        format!("https://{host}/v2")
    };
    Ok((base_url, app_key, app_secret))
}

/// 从令牌端点的**响应体**里取身份信息：一发里有两枚 JWT，名字可能装在任意一枚里。
///
/// ── 为什么要看 `refresh_token` ──────────────────────────────
/// 实测（2026-10-11）：这个上游把身份放在 **`refresh_token`** 的 `user_profile` 里
/// —— `account_name`（IAM 用户名）、`principal_id`、`account_id` 都在。只读
/// `id_token` 的话账号名永远退到十六进制兜底。
///
/// 顺序：`id_token` 优先（它是这次会话的身份快照），它没有名字再退
/// `refresh_token`。principal_id 同口径，但**不因名字空而被丢弃**。
fn response_identity(payload: &Value) -> TokenIdentity {
    let mut principal_only = TokenIdentity::default();
    for field in ["id_token", "refresh_token"] {
        let identity = jwt_identity(payload.get(field).and_then(Value::as_str).unwrap_or(""));
        if !identity.user_name.is_empty() {
            return identity;
        }
        if principal_only.principal_id.is_empty() && !identity.principal_id.is_empty() {
            principal_only = identity;
        }
    }
    principal_only
}

/// 令牌 claims 解出来的身份信息。两项都**可能为空** —— 取不到不是错误。
#[derive(Clone, Default)]
pub struct TokenIdentity {
    pub user_name: String,
    pub principal_id: String,
}

/// 从一枚 JWT（`id_token` 或 `refresh_token`）的 payload 里取显示名与 principal id。
///
/// ── 为什么要比参考实现多解一层 ──────────────────────────────
/// 参考实现只读顶层 `preferred_username || name`。**实测这个租户的令牌顶层没有
/// 这两个键** —— 身份装在一个 base64 JSON 串里：
/// ```text
/// user_profile → { account_id, account_name, principal_id, principal_urn, ... }
/// ```
/// 所以这里的取法是**顶层优先、内层兜底**。全程 fail-open：解不出就当没取到，
/// 拿它报错会把一次已经成功的登录记成失败。
pub fn jwt_identity(token: &str) -> TokenIdentity {
    let Some(claims) = jwt_claims(token) else {
        return TokenIdentity::default();
    };
    let profile = claims
        .get("user_profile")
        .and_then(Value::as_str)
        .and_then(decode_user_profile);
    let principal_id = profile_text(&profile, "principal_id")
        .or_else(|| text_of(&claims, "principal_id"))
        .unwrap_or_default();
    // 顶层字段优先：上游哪天把名字放回顶层，这里不需要改代码
    for field in ["preferred_username", "name"] {
        if let Some(text) = text_of(&claims, field) {
            return TokenIdentity {
                user_name: text,
                principal_id,
            };
        }
    }
    TokenIdentity {
        user_name: profile_text(&profile, "account_name").unwrap_or_default(),
        principal_id,
    }
}

/// 解 JWT 的 payload 段。**不验签** —— 这里只取显示名，不用于任何鉴权判断。
fn jwt_claims(id_token: &str) -> Option<Value> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let mut parts = id_token.split('.');
    parts.next()?; // header
    let payload = parts.next()?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('=').as_bytes())
        .ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()
}

/// 解 `user_profile`：一段**无填充** base64 的 JSON。两种字母表都试
/// （URL_SAFE 与 STANDARD），谁先解出合法 JSON 算谁 —— 上游在这里没给口径，
/// 而这条链的代价是「取不到名字」，不是「取错名字」。
fn decode_user_profile(text: &str) -> Option<Value> {
    use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
    use base64::Engine;
    let body = text.trim().trim_end_matches('=');
    if body.is_empty() {
        return None;
    }
    for candidate in [URL_SAFE_NO_PAD.decode(body), STANDARD_NO_PAD.decode(body)] {
        let Ok(bytes) = candidate else { continue };
        if let Ok(parsed) = serde_json::from_slice::<Value>(&bytes) {
            return Some(parsed);
        }
    }
    None
}

/// 从已解开的 `user_profile` 里取一个非空字符串字段。
fn profile_text(profile: &Option<Value>, field: &str) -> Option<String> {
    profile.as_ref().and_then(|value| text_of(value, field))
}

/// 取一个非空的字符串字段（trim 后为空算没有）。
fn text_of(container: &Value, field: &str) -> Option<String> {
    container
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn text_in(container: &Value, field: &str) -> String {
    text_of(container, field).unwrap_or_default()
}

/// 把 RFC3339（`2026-10-10T05:15:00Z`）解析成毫秒时间戳。
pub(super) fn parse_rfc3339_ms(input: &str) -> Option<i64> {
    let text = input.trim().trim_end_matches('Z');
    let (date, time) = text.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let second: i64 = time_parts
        .next()
        .and_then(|value| value.split('.').next())
        .unwrap_or("0")
        .parse()
        .ok()?;
    days_from_civil(year, month, day)
        .map(|days| (days * 86_400 + hour * 3600 + minute * 60 + second) * 1000)
}

/// Howard Hinnant 的 `days_from_civil`（年月日 → epoch 起的天数）。
fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let adjusted_year = if month <= 2 { year - 1 } else { year };
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let yoe = adjusted_year - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}
