//! Antigravity 的 Google OAuth：**授权码换 token（网页登录）** +
//! `grant_type=refresh_token` 刷新（同一个 token 端点）。
//!
//! ── 协议（规格 §1.5，两套参考实现逐字一致）────────────────────
//! ```text
//! POST https://oauth2.googleapis.com/token
//! Content-Type: application/x-www-form-urlencoded
//! User-Agent: vscode/1.X.X (Antigravity/{ver})        ← 原生 OAuth UA
//!
//! client_id     = <内置的公开客户端 id>
//! client_secret = <同上>
//! refresh_token = <账号记录里的主凭证>
//! grant_type    = refresh_token                      ← 下划线形态，别写成 refreshToken
//!
//! 200 → {access_token, expires_in(秒), token_type, refresh_token?, id_token?}
//! ```
//! **`refresh_token` 通常不回传**（Google 只在首次授权时下发）：那时保留旧值，
//! 绝不把已有字段洗成空（与 Trae / Qoder 的同一条规矩）。
//!
//! ── 网页登录：授权码 + loopback（规格 §1.1–§1.4）───────────────
//! 与别家的网页登录同形（授权页 → 回调 → 换码 → 落账号），Google 侧的三步
//! 逐字取自参考实现（`Antigravity-Manager/src-tauri/src/modules/oauth.rs` +
//! `oauth_server.rs`）：
//! ```text
//! ① 授权页  https://accounts.google.com/o/oauth2/v2/auth
//!      client_id / redirect_uri / response_type=code
//!      scope = 6 个空格连接（openid、cloud-platform、userinfo.email、
//!              userinfo.profile、cclog、experimentsandconfigs）
//!      access_type=offline + prompt=consent   ← 保证下发 refresh_token
//!      include_granted_scopes=true + state=<一次性随机串>
//! ② 回调    http://localhost:{网关端口}/oauth-callback
//!          （参考实现双栈可绑定时逐字用 `localhost`，路径 `/oauth-callback`；
//!           Google 的 desktop 型 client 允许 loopback 任意端口）
//! ③ 换码    POST https://oauth2.googleapis.com/token（form）
//!      client_id / client_secret / code
//!      redirect_uri（与 ① 逐字相同！）/ grant_type=authorization_code
//! ```
//! `redirect_uri` 的端口来自进程级常量（[`set_loopback_port`]，bootstrap 写一次）：
//! `ProviderAdapter::build_login_url()` 是同步无参的 trait 契约，拿不到
//! `ServerState`，因此照 accio / codearts 的先例开机写一次、之后只读。
//! **没有 PKCE、没有设备码**（规格 §1.2）：state 是这条链路上唯一的一次性
//! CSRF 凭据，由适配器用 `raccoon::oauth::new_login_state()` 生成（本仓唯一的
//! 不可预测随机源），逐字比对在 `core::login::submit_login_callback` 里做。
//!
//! userinfo（`GET .../oauth2/v2/userinfo`，Bearer）只为取 `email`：它是展示名
//! 与账号身份（`account_store::antigravity_accounts` 的 id 优先按 email 派生）。
//! 失败**不阻断落账号** —— 取不到就空着，id 退到令牌摘要（v2 失败再试 v1，
//! 规格 §8.4：两套参考各用一个版本，都能返回 email）。
//!
//! ── `invalid_grant` 的处置（与规格 §1.6 的差异，有意）───────────
//! Manager 收到 `invalid_grant` 会**停用账号**（写 `disabled: true` 并摘出
//! token 池）。本仓没有「停用」这条产品路径，既有几家的口径是：**报一个
//! 可识别的永久性错误，让用户重新登录/重新粘贴凭证**（Cline 的
//! 「登录态已失效（refreshToken 被拒绝），请重新登录」、Qoder 的
//! 「刷新令牌已失效或与所选地区不符，请重新登录该账号」）。本家照做：
//!   - `invalid_grant` → 401 + 「refresh token 已失效或被撤销，请重新授权后粘贴
//!     新的 refresh_token」；
//!   - `invalid_client` / `unauthorized_client` → 401 + 「OAuth 客户端未被接受」
//!     （本仓只用内置的那一个 client，多 client 轮询是 Manager 的特有能力，
//!     本步不做 —— 规格 §1.5 的「client 不匹配处理」列为 TODO）；
//!   - 其余失败 → 502（上游挂 / 网络问题，可重试），文案带上游原文（截断）。
//! 不做「500ms 退避重试确认」：那是 Manager 为了区分抖动与真失效加的，
//! 而本仓的永久性错误不会被自动重试（只在用户点刷新或下次转发时再走一遍），
//! 多打一次请求没有收益。
//!
//! ── 单飞 + 比较再写（与 Trae / Cline 同一纪律）──────────────────
//! 刷新是秒级网络动作：并发请求会各自去打一次（Google 侧对同一 refresh_token
//! 的并发刷新没有硬限制，但白打没有意义），且回写必须「比较再写」——
//! 期间用户可能重新粘贴凭证，无条件覆盖会把新凭证盖掉。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;

use agent2api_provider_protocol::WireProxy as ResolvedProxy;
use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;
use crate::flight::{self, Join, Table};
use crate::http as egress;
use crate::logging;
use crate::proxy_util::{resolve_account_proxy, ProxyResolution};

use super::credentials::AntigravityCredentials;
use super::endpoints;
use super::project;

/// token 端点请求超时（与别家的 30 秒同档）
const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 上游没给 `expires_in` 时的保守默认（Google 实测约 1 小时；给短了只会多刷一次）
const DEFAULT_EXPIRES_IN_SECONDS: i64 = 3600;

/// 上游错误文在报错里的截断长度（Google 的错误体可能很长）
const MAX_ERROR_CHARS: usize = 300;

/// 本家登录回调地址的路径（授权时拼进授权 URL、换码时逐字回传）。
///
/// ── 迁移时改了什么 ──────────────────────────────────────────
/// 网关那版把 `http://localhost:{网关端口}/oauth-callback` 写进进程级常量
/// （`set_loopback_port`），因为 `build_login_url()` 是同步无参的 trait 契约。
/// 插件的 `login_start` 是 async 且能自己起监听器，所以回调地址改成**按轮
/// 传入**（[`build_authorize_url`] 的 `callback_url` 参数）—— 一轮一个
/// loopback 端口，`state` 仍然逐字比对（见 `login.rs`）。
pub const CALLBACK_PATH: &str = "/oauth-callback";

/// 授权码长度上限（Google 的 code 很短；给一个防呆上限而不是信任输入）
const MAX_CODE_LENGTH: usize = 8192;

/// state 长度上限（本仓生成的 uuid 形态远短于此；防的是手工粘贴的长串）
const MAX_STATE_LENGTH: usize = 512;

/// userinfo 请求超时（它是顺手取一次 email，不该拖住落账号）
const USERINFO_TIMEOUT_MS: u64 = 20_000;

/// 拼 Google 授权地址（规格 §1.2；参数与顺序照参考实现的
/// `get_auth_url_with_client` 逐字）。
///
/// 参数表：
/// ```text
///   client_id              内置的公开客户端（endpoints::CLIENT_ID）
///   redirect_uri           调用方给的本轮 loopback 地址（`login.rs` 现绑端口）
///   response_type          code
///   scope                  6 个 scope 空格连接（endpoints::SCOPES，顺序照参考）
///   access_type            offline   ← 与 prompt=consent 一起保证下发 refresh_token
///   prompt                 consent
///   include_granted_scopes true      ← Manager 有、9router 无（非必需，照 Manager）
///   state                  一次性随机串（生成与比对见 `login.rs`）
/// ```
///
/// `callback_url` 由调用方给：换码时回传的 `redirect_uri` 必须是**同一个串**
/// （Google 逐字比对），所以两处都取自调用方手里那一份。
pub fn build_authorize_url(callback_url: &str, state: &str) -> String {
    let mut url = match url::Url::parse(endpoints::AUTH_URL) {
        Ok(url) => url,
        // 常量写错才会走到这里：返回一个空串，调用方按「没能生成授权地址」处理
        Err(_) => return String::new(),
    };
    url.query_pairs_mut()
        .append_pair("client_id", endpoints::CLIENT_ID)
        .append_pair("redirect_uri", callback_url)
        .append_pair("response_type", "code")
        .append_pair("scope", &endpoints::SCOPES.join(" "))
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .append_pair("include_granted_scopes", "true")
        .append_pair("state", state);
    url.to_string()
}

/// 单飞表（键 = 账号文件 + 账号 id + refresh_token 指纹，见 [`ensure_fresh`]）
static FLIGHTS: OnceLock<Table<AntigravityCredentials>> = OnceLock::new();

/// 一次 token 端点调用的结果
#[derive(Clone, Debug)]
pub struct TokenResponse {
    /// 新的访问令牌
    pub access_token: String,
    /// 新的 refresh_token（**通常为 None** —— Google 不回传时保留旧值）
    pub refresh_token: Option<String>,
    /// 有效期（秒）
    pub expires_in: i64,
    /// `token_type`（`Bearer`）
    pub token_type: String,
}

/// 账号级出口代理（解析失败按 400 报，不静默直连）—— 与 `trae::adapter` /
/// `accio::auth` 里同名函数同一语义、同一文案来源。
pub fn account_proxy(record: &Value) -> Result<Option<ResolvedProxy>, GatewayError> {
    match resolve_account_proxy(record.get("proxy")) {
        Some(ProxyResolution::Resolved(proxy)) => Ok(Some(proxy)),
        Some(ProxyResolution::Failed(reason)) => Err(GatewayError::with_status(400, reason)),
        None => Ok(None),
    }
}

/// 截断上游错误文（按字符，UTF-8 安全）
fn truncate(text: &str) -> String {
    let collapsed: String = text.chars().filter(|character| !character.is_control()).collect();
    match collapsed.char_indices().nth(MAX_ERROR_CHARS) {
        Some((index, _)) => format!("{}…", &collapsed[..index]),
        None => collapsed,
    }
}

/// 上游错误体里的 `error` / `error_description`（非 JSON 时给空）
fn upstream_error(payload: Option<&Value>) -> (String, String) {
    let Some(payload) = payload else {
        return (String::new(), String::new());
    };
    let code = payload
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let description = payload
        .get("error_description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    (code, description)
}

/// 解析 token 端点的一位读数（`expires_in` 接受数字与数字字符串两种形态）
fn expires_in_of(payload: &Value) -> i64 {
    match payload.get("expires_in") {
        Some(Value::Number(number)) => number.as_i64().unwrap_or(0),
        Some(Value::String(text)) => text.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

/// 解析成功响应（200 且带 `access_token`）。
///
/// 刷新与换码的响应形态相同（`TokenResponse`），差的只是 `access_token` 缺失
/// 时的文案语境（刷新 =「旧凭证未被覆盖」，换码 =「账号未保存」），因此由调用
/// 方把文案递进来。
fn read_token_response(payload: &Value, missing_access_token: &str) -> Result<TokenResponse, GatewayError> {
    let access_token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or_else(|| GatewayError::with_status(502, missing_access_token))?;
    let refresh_token = payload
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let expires_in = expires_in_of(payload);
    let token_type = payload
        .get("token_type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or("Bearer")
        .to_string();
    Ok(TokenResponse {
        access_token,
        refresh_token,
        expires_in,
        token_type,
    })
}

/// 解析刷新响应（`refresh_access_token` 的出口；文案见 [`read_token_response`]）
fn parse_token_response(payload: &Value) -> Result<TokenResponse, GatewayError> {
    read_token_response(payload, "Antigravity 刷新响应缺少 access_token，旧凭证未被覆盖")
}

/// 把一次失败响应翻成网关错误（见模块头的三档处置）。
fn classify_failure(status: u16, text: &str, payload: Option<&Value>) -> GatewayError {
    let (code, description) = upstream_error(payload);
    let detail = if description.is_empty() {
        truncate(text)
    } else {
        truncate(&description)
    };
    match code.as_str() {
        "invalid_grant" => GatewayError::with_status(
            401,
            "Antigravity 的 refresh token 已失效或被撤销（Google 返回 invalid_grant）：\
             请在 Google 账号授权页重新授权后，把新的 refresh_token 粘贴进本账号",
        ),
        "invalid_client" | "unauthorized_client" => GatewayError::with_status(
            401,
            format!(
                "Antigravity 的 OAuth 客户端未被 Google 接受（{code}）：\
                 本网关内置的 client 与该 refresh token 不匹配，请重新授权后粘贴新的 refresh_token"
            ),
        ),
        _ => {
            let hint = if detail.is_empty() {
                String::new()
            } else {
                format!("：{detail}")
            };
            GatewayError::with_status(502, format!("Antigravity 刷新 token 失败（{status}）{hint}"))
        }
    }
}

/// 打一次 token 端点（**纯网络 + 解析，不落盘**）。
///
/// 日志只打「成功/失败 + 状态码」，**绝不打印 token 本体或表单内容**
/// （refresh_token 是账号主凭证）。
pub async fn refresh_access_token(
    refresh_token: &str,
    proxy: Option<&ResolvedProxy>,
) -> Result<TokenResponse, GatewayError> {
    let refresh_token = refresh_token.trim();
    if refresh_token.is_empty() {
        return Err(GatewayError::with_status(
            401,
            "Antigravity 账号缺少 refresh token，请重新粘贴（Google OAuth 的 refresh_token）",
        ));
    }
    let client = super::client_for(proxy);
    let response = client
        .post(endpoints::TOKEN_URL)
        .header("User-Agent", endpoints::oauth_user_agent())
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        // `Accept: application/json` 与别家的管理接口一致；token 端点本就回 JSON
        .header("Accept", "application/json")
        .form(&[
            ("client_id", endpoints::CLIENT_ID),
            ("client_secret", endpoints::CLIENT_SECRET),
            ("refresh_token", refresh_token),
            ("grant_type", endpoints::GRANT_TYPE_REFRESH),
        ])
        .send()
        .await
        .map_err(|error| {
            // 连接类失败（连不上 / 超时）在 Google 上游几乎都指向「出网路径不通」：
            // 带上代理提示，用户才知道该往哪查（见 `super::client_for` 的口径）。
            let hint = if error.is_connect() {
                "（本机访问 Google 需经代理时：请开启系统代理，或给该账号配置出网代理）"
            } else {
                ""
            };
            GatewayError::with_status(
                502,
                format!(
                    "Antigravity 刷新请求失败：{}{hint}",
                    egress::describe_chain(&error)
                ),
            )
        })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let error = classify_failure(status, &text, Some(&payload));
        logging::log(
            "[Antigravity]",
            &format!("❌ 刷新 token 失败（HTTP {status}）：{}", error.message),
        );
        return Err(error);
    }
    parse_token_response(&payload)
}

// ─── 网页登录：授权码 → 换 token → 落账号 ────────────────────

/// 换码失败的处置。
///
/// 与刷新那条 [`classify_failure`] 分开：同一个 `invalid_grant` 在两条链路上
/// 指的是不同的事（刷新时 = refresh_token 被撤销，换码时 = 授权码过期 / 已用
/// 过 / redirect_uri 与授权时不一致），照做的动作也不同（前者重新粘贴凭证，
/// 后者重新发起网页登录）。
fn classify_exchange_failure(status: u16, text: &str, payload: Option<&Value>) -> GatewayError {
    let (code, description) = upstream_error(payload);
    let detail = if description.is_empty() {
        truncate(text)
    } else {
        truncate(&description)
    };
    let hint = if detail.is_empty() {
        String::new()
    } else {
        format!("：{detail}")
    };
    match code.as_str() {
        "invalid_grant" => GatewayError::with_status(
            400,
            format!("Antigravity 授权码已失效或已被使用，请重新发起网页登录{hint}"),
        ),
        "redirect_uri_mismatch" => GatewayError::with_status(
            400,
            format!("Antigravity 授权回调地址与发起时不一致，请重新发起网页登录{hint}"),
        ),
        "invalid_client" | "unauthorized_client" => GatewayError::with_status(
            401,
            format!("Antigravity 的 OAuth 客户端未被 Google 接受（{code}）：请重新发起网页登录{hint}"),
        ),
        _ => GatewayError::with_status(
            502,
            format!("Antigravity 网页登录换取凭证失败（{status}）{hint}"),
        ),
    }
}

/// 取 userinfo 的 `email`（best-effort）：v2 失败回落 v1，两个版本都失败返回
/// None。调用点把它当纯展示 / 身份增强，**绝不因它失败而判登录失败**。
///
/// 日志口径与刷新一致：只记成败，不打印令牌本体。
async fn fetch_email(access_token: &str, proxy: Option<&ResolvedProxy>) -> Option<String> {
    let token = access_token.trim();
    if token.is_empty() {
        return None;
    }
    // 有这一轮登录的代理就用它；没有才跟随系统代理（`client_for` 的口径见
    // 那个函数的文档）。登录发生在账号存在之前，没有账号级代理可挂，所以
    // 「用户在登录弹窗里选的代理」是这一层唯一能拿到的出口。
    let client = super::client_for(proxy);
    let (auth_key, auth_value) = endpoints::bearer_header(token);
    for url in [endpoints::USERINFO_URL_V2, endpoints::USERINFO_URL_V1] {
        let response = client
            .get(url)
            .header("User-Agent", endpoints::oauth_user_agent())
            .header("Accept", "application/json")
            .header(auth_key.clone(), auth_value.clone())
            .timeout(Duration::from_millis(USERINFO_TIMEOUT_MS))
            .send()
            .await;
        let Ok(response) = response else { continue };
        if !response.status().is_success() {
            continue;
        }
        let text = response.text().await.unwrap_or_default();
        let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let email = payload
            .get("email")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if email.is_some() {
            return email;
        }
    }
    None
}

/// **网页登录**的收尾：授权码 → token 端点换凭证 → best-effort 取 email /
/// project → 产出可落库的账号。
///
/// ── 迁移时改了什么（网关 → 插件）───────────────────────────
/// 网关那版直接 `store.add_antigravity_account(...)` 落库并返回账号 id；
/// 插件侧账号由**网关**落库（`login.poll` 返回 `NormalizedAccount`，网关负责
/// 撞 id 保护与优先级分配）。所以这里只负责换码 + 补料，产出交给
/// [`super::accounts::account_of`] 归一化。
///
/// ── `redirect_uri` 从哪来 ───────────────────────────────────
/// 由调用方（`login.rs`）把它发起那一轮的 loopback 地址传进来 —— 必须与
/// 授权时拼进 URL 的那份**逐字相同**（Google 会比对，差一个字符就
/// `redirect_uri_mismatch`）。
///
/// ── 失败语义 ────────────────────────────────────────────────
///   - 授权码失效 / redirect_uri 不一致 → 400（不可重试，重新发起）；
///   - OAuth 客户端被拒 → 401；其余上游失败 → 502（可重试）；
///   - 换到了 token 但**没有 refresh_token** → 400：那种账号落下来也没有
///     续期手段（refresh_token 才是本家主凭证），必须让用户照做（Google 只在
///     `prompt=consent` + `access_type=offline` 的**首次**授权下发，重复授权
///     可在 Google 账号的「第三方访问」里撤销后重试）。
pub async fn exchange_code(
    code: &str,
    state: &str,
    redirect_uri: &str,
    proxy: Option<&ResolvedProxy>,
) -> Result<super::credentials::AntigravityCredentials, GatewayError> {
    let code = code.trim();
    if code.is_empty() {
        return Err(GatewayError::with_status(400, "缺少授权码，请重新发起网页登录"));
    }
    if code.chars().count() > MAX_CODE_LENGTH {
        return Err(GatewayError::with_status(
            400,
            "授权码过长，请确认复制的是回调 URL 里的 code",
        ));
    }
    let state = state.trim();
    if state.is_empty() {
        return Err(GatewayError::with_status(
            400,
            "缺少登录 state，无法确认这次登录由本机发起，请重新发起网页登录",
        ));
    }
    if state.chars().count() > MAX_STATE_LENGTH {
        return Err(GatewayError::with_status(400, "登录 state 过长，请重新发起网页登录"));
    }
    let redirect_uri = redirect_uri.trim();
    if redirect_uri.is_empty() {
        return Err(GatewayError::with_status(
            500,
            "这一轮登录的回调地址丢了，请重新发起网页登录",
        ));
    }
    // 换码走**这一轮登录**的代理（用户在登录弹窗里选的那条）；没选才跟随
    // 系统代理。无头服务器上没有系统代理，选代理是唯一能出 Google 的路径。
    let client = super::client_for(proxy);
    let response = client
        .post(endpoints::TOKEN_URL)
        .header("User-Agent", endpoints::oauth_user_agent())
        .timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
        .header("Accept", "application/json")
        .form(&[
            ("client_id", endpoints::CLIENT_ID),
            ("client_secret", endpoints::CLIENT_SECRET),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("grant_type", endpoints::GRANT_TYPE_AUTHORIZATION_CODE),
        ])
        .send()
        .await
        .map_err(|error| {
            // 与刷新同款：连接类失败带上代理提示（用户报过 os error 10060 ——
            // 直连 Google 超时，正是这条提示要指出的场景）
            let hint = if error.is_connect() {
                "（本机访问 Google 需经代理时：请开启系统代理后重试）"
            } else {
                ""
            };
            GatewayError::with_status(
                502,
                format!(
                    "Antigravity 网页登录换码请求失败：{}{hint}",
                    egress::describe_chain(&error)
                ),
            )
        })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let error = classify_exchange_failure(status, &text, Some(&payload));
        logging::log(
            "[Antigravity]",
            &format!("❌ 网页登录换取凭证失败（HTTP {status}）：{}", error.message),
        );
        return Err(error);
    }
    let token = read_token_response(&payload, "Antigravity 换码响应缺少 access_token，账号未保存")?;
    let Some(refresh_token) = token
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
    else {
        return Err(GatewayError::with_status(
            400,
            "Google 未返回 refresh token（该授权此前完成过，重复授权不会再次下发）：\
             请在 Google 账号的「第三方访问 / 已授权的应用」里撤销本应用后重新发起网页登录",
        ));
    };
    let access_token = token.access_token.trim().to_string();
    // `expires_in` 越界（上游给了脏值）时按默认 1 小时算 —— 与刷新链路同一条处置
    let expires_in = if (1..=7 * 24 * 3600).contains(&token.expires_in) {
        token.expires_in
    } else {
        DEFAULT_EXPIRES_IN_SECONDS
    };
    let expires_at = logging::now_ms() + expires_in * 1000;
    // email / project 都是 best-effort：失败只告警，不影响账号可用性
    let email = fetch_email(&access_token, proxy).await.unwrap_or_default();
    if email.is_empty() {
        logging::verbose(
            "[Antigravity]",
            "网页登录未取到 email（账号照常保存，展示名与身份退到令牌摘要）",
        );
    }
    let project_id = match project::discover_project(&access_token, proxy).await {
        Ok(found) => found,
        Err(error) => {
            logging::verbose(
                "[Antigravity]",
                &format!("网页登录 project 未发现（不影响账号添加）：{}", error.message),
            );
            String::new()
        }
    };
    // 产出凭证对象：键名与 `credentials::from_record` 的读取对齐
    // （refreshToken / accessToken / expiresAt / projectId / email），
    // 落库由调用方的 `accounts::account_of` 负责（插件不直接写账号库）。
    logging::log(
        "[Antigravity]",
        &format!(
            "✅ 网页登录换取凭证成功（{}{}）",
            if email.is_empty() {
                "email 未取到".to_string()
            } else {
                format!("email {email}")
            },
            if project_id.trim().is_empty() {
                "，project 未发现（刷模型清单时会再试）".to_string()
            } else {
                format!("，project {project_id}")
            }
        ),
    );
    Ok(AntigravityCredentials {
        // 身份 / 备注名由 `accounts::account_of` 按 email → 令牌摘要派生
        id: String::new(),
        refresh_token,
        access_token,
        expires_at,
        project_id,
        email,
        name: String::new(),
    })
}

/// 取可用凭证（`force = true` 时不看临期窗口，401 之后强制刷一次）。
///
/// `account_id` 为空 = 本家的队首可用账号（与别家同口径；点名的账号取不到
/// 直接报错，不回落队首 —— 那会变成「点名了 A、用了 B」）。
pub async fn ensure_fresh(
    ctx: &PluginCtx,
    account_id: &str,
    force: bool,
) -> Result<AntigravityCredentials, GatewayError> {
    let record = read_record(ctx, account_id).await?;
    let credentials = super::credentials::from_record(Some(&record))?;
    if !force && !credentials.expiring() {
        return Ok(credentials);
    }
    if !credentials.can_refresh() {
        return Err(GatewayError::with_status(
            401,
            "Antigravity 账号缺少 refresh token，无法续期，请重新粘贴",
        ));
    }
    // 单飞键：插件只有一个进程、一个账号库，所以不需要网关那份
    // `file_string()` 前缀（那是为了区分同一进程里挂多个库的情形）。
    let key = format!(
        "antigravity:{}:{}",
        account_id,
        flight::fingerprint(&credentials.refresh_token)
    );
    match FLIGHTS.get_or_init(Table::new).join(&key) {
        Join::Waiter(waiter) => waiter.wait().await,
        Join::Leader(leader) => {
            let result = refresh_and_save(ctx, &record, &credentials).await;
            leader.finish(result.clone());
            result
        }
    }
}

/// 打一次刷新 + 回写（单飞的 leader 走这一段）。
///
/// ── 回写的四处取值（`None`/空值一律**保留旧值**）───────────────
///   - `access_token`：新令牌（必有）；
///   - `refresh_token`：上游回传才覆盖（见模块头）；
///   - `expiresAt`：`now + expires_in`（规格 §2 的 `expiry_timestamp` 口径）；
///   - `projectId`：本次**刷到了**才写（见下）。
///
/// ── project 的发现时机 ──────────────────────────────────────
/// `project` 只影响聊天请求（规格 §3.4：目录 / 额度 / loadCodeAssist 都忽略它），
/// 因此这里只做**一次** best-effort 补齐：账号记录里没有
/// projectId 时顺手发现一次，失败只打日志、不影响刷新结果（用户加账号时
/// `login.rs` 不阻塞同一条路径）。
async fn refresh_and_save(
    ctx: &PluginCtx,
    record: &Value,
    credentials: &AntigravityCredentials,
) -> Result<AntigravityCredentials, GatewayError> {
    let proxy = account_proxy(record)?;
    let response = refresh_access_token(&credentials.refresh_token, proxy.as_ref()).await?;
    let mut fresh = credentials.clone();
    fresh.access_token = response.access_token.trim().to_string();
    if let Some(refresh) = response.refresh_token.as_ref() {
        fresh.refresh_token = refresh.trim().to_string();
    }
    // `expires_in` 越界（上游给了脏值）时按默认 1 小时算：给短了只会多刷一次，
    // 给长了会让失效令牌留在「不临期」状态 —— 两者都比信任一个脏值好。
    let expires_in = if (1..=7 * 24 * 3600).contains(&response.expires_in) {
        response.expires_in
    } else {
        DEFAULT_EXPIRES_IN_SECONDS
    };
    fresh.expires_at = logging::now_ms() + expires_in * 1000;
    logging::verbose(
        "[Antigravity]",
        &format!(
            "token 刷新成功（token_type {}，有效期 {expires_in} 秒）",
            response.token_type
        ),
    );
    // best-effort：project 缺失时补一次（只影响聊天，失败不阻断刷新）
    let mut project_id: Option<String> = None;
    if credentials.project_id.trim().is_empty() {
        if let Ok(found) = project::discover_project(&fresh.access_token, proxy.as_ref()).await {
            if !found.trim().is_empty() {
                project_id = Some(found.clone());
                fresh.project_id = found;
            }
        }
    }
    // ── 写回：比较再写（`expect` 是读到时的 secret）─────────────────
    // 网关那版 `update_antigravity_credentials_if_current` 逐键比对 5 个字段；
    // 插件侧统一走 `store::save`（值没变才写）。空值**不覆盖**：
    //   · `accessToken` 必有（刷新成功才走到这里）；
    //   · `refreshToken` 上游回传才覆盖（Google 通常不回传，见模块头）；
    //   · `expiresAt` 用本次算出的值；
    //   · `projectId` 本次发现了才写。
    let expect = crate::store::secret_view(record);
    let mut fields: Vec<(&str, Value)> = vec![
        ("accessToken", Value::String(fresh.access_token.clone())),
        ("expiresAt", Value::from(fresh.expires_at)),
    ];
    if let Some(refresh) = response.refresh_token.as_deref().map(str::trim).filter(|text| !text.is_empty()) {
        fields.push(("refreshToken", Value::String(refresh.to_string())));
        fields.push(("tokenTail", Value::String(crate::store::token_tail(refresh))));
    }
    if let Some(project) = project_id.as_deref().map(str::trim).filter(|text| !text.is_empty()) {
        fields.push(("projectId", Value::String(project.to_string())));
    }
    let next = crate::store::with_fields(&expect, &fields);
    let id = record.get("id").and_then(Value::as_str).unwrap_or("");
    if id.is_empty() {
        return Err(GatewayError::with_status(
            409,
            "Antigravity 续期结果无法写回（账号记录缺 id），请重新添加该账号",
        ));
    }
    if crate::store::save(ctx, id, &expect, &next).await? {
        return Ok(fresh);
    }
    // 记录在刷新期间被换过（重新粘贴 / 手工编辑）：把已刷出的令牌用掉，
    // 但不覆盖记录，并如实说一句让用户知道（与 Trae 同一处置）。
    logging::log(
        "[Antigravity]",
        "⚠️ 刷新结果未能写回账号记录（期间被改动过），本次请求用新令牌，下次会以记录里的为准",
    );
    Ok(fresh)
}

/// 读账号记录（点名 vs 队首）。
///
/// 点名的账号不存在 → 报错，**不回落队首**：那会把「点名的账号坏了」变成
/// 「静默用了别人的额度」（与 Trae / ZCode 同一条纪律）。
pub async fn read_record(ctx: &PluginCtx, account_id: &str) -> Result<Value, GatewayError> {
    crate::store::record(ctx, super::PROVIDER_ID, account_id).await
}
