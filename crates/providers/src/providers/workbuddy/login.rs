//! WorkBuddy 的浏览器登录，改造成插件协议的「轮询型」。
//!
//! ── 链路（四步，对照网关 `core::login` 的 `login_interactive`）─────
//! ```text
//!   ① POST {endpoint}/v2{prefix}/auth/state?platform=<platform>   （匿名）
//!        → data.state + data.authUrl
//!   ② 用户在浏览器里打开 authUrl 完成登录
//!   ③ GET  {endpoint}/v2{prefix}/auth/token?state=<state>          （匿名）
//!        → accessToken / refreshToken / expiresIn / refreshExpiresIn
//!        未完成时上游回 code=11217，继续轮询
//!   ④ GET  {endpoint}/v2{prefix}/login/account?state=<state>       （Bearer）
//!        → 账号详情（失败不影响登录，回退用 ⑤ 的列表）
//!   ⑤ GET  {endpoint}/v2{prefix}/accounts                          （Bearer）
//!        → data.accounts[]
//! ```
//!
//! ④⑤ 两步只为**补齐昵称等资料**：身份本身在 token 里（`sub`），所以两步全挂
//! 时登录仍然成功（三级兜底见 [`account_from_token_claims`]）。
//!
//! ── 与网关那份的差别（迁移时改的）───────────────────────────
//! 网关的 `LoginService::start` 起一个**阻塞到结束**的后台任务；插件协议里
//! 登录是「开始 → 反复轮询」两段，所以拆成：
//!
//! | 网关 | 插件 |
//! |---|---|
//! | `login_interactive` 起 spawn 任务后 while 轮询 | [`login_start`] 只做 ①②，[`login_poll`] 每次轮询**一次** |
//! | `store.add_account(&session, …)` | 返回 `NormalizedAccount`，由网关落库 |
//! | `AuthService::fetch_login_account` / `fetch_accounts` | 本文件的 `fetch_login_account` / `fetch_accounts` |
//! | `context_for_edition(region)` | [`Context::make`] + 地区常量（同一个端点事实来源） |
//!
//! ── 流程状态放哪 ────────────────────────────────────────────
//! 每次轮询都要知道「打哪个 state、哪个地区」。插件是单进程长驻的，所以放
//! 进程内的表里（键 = state，与网关「按 state 找任务」同一口径）。条目存
//! `Arc<Flow>`：`poll` 是 async，`MutexGuard` 不能跨 await 持有。
//!
//! ── 端点覆盖 ────────────────────────────────────────────────
//! `WORKBUDDY_ENDPOINT` / `WORKBUDDY_INTL_ENDPOINT`（以及旧组合
//! `WORKBUDDY_EDITION=intl`）仍然生效 —— 那是 staging / 自建网关用户的
//! 落点，属于「打哪个站」的知识，随实现一起留在插件里。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use agent2api_provider_protocol as proto;

use crate::errors::GatewayError;
use crate::http::{send_raw, ApiResponse};
use crate::logging;
use crate::workbuddy_shared::endpoints::{
    Context, ANONYMOUS_HEADERS, RESPONSE_CODE_OK, SERVER_CODES_RETRY_FETCH_TOKEN,
};

use super::accounts::{account_view, identity_of};
use super::region::Region;

/// 轮询间隔（与网关 `LOGIN_POLL_INTERVAL_MS` 同值：桌面端 SIGN_IN_FETCH_INTERVAL）。
pub const POLL_INTERVAL_MS: u64 = 3_000;

/// 登录接口的超时（网关 `auth_http::REQUEST_TIMEOUT_MS` 同值）。
const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// 流程表条目的保留时长。网关的任务表按 5 分钟总超时收尾，这里留 10 分钟
/// —— 比它更老的 flow 不可能还有人问。
const FLOW_KEEP_MS: i64 = 10 * 60_000;

/// 一次进行中的登录（`state` + 它属于哪个地区）。
struct Flow {
    region: Region,
    /// 发起时刻（毫秒），清理用。
    started_at: i64,
}

/// 流程表：`state` → 这次登录的上下文。
fn flows() -> &'static Mutex<HashMap<String, Flow>> {
    static FLOWS: OnceLock<Mutex<HashMap<String, Flow>>> = OnceLock::new();
    FLOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 发起登录：问上游要 state + authUrl。
pub(super) async fn login_start(region: Region) -> Result<proto::LoginStart, GatewayError> {
    let context = context_for(region);
    let headers = anonymous_headers();
    let url = context.auth_url(&format!("/auth/state?platform={}", encode(&context.platform)));
    let response = send("POST", &url, Some(&json!({})), &headers).await?;
    let data = unwrap(&response, "auth/state")?;
    let state = data
        .get("state")
        .or_else(|| data.get("authState"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    let auth_url = data
        .get("authUrl")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    if state.is_empty() || auth_url.is_empty() {
        return Err(GatewayError::with_status(
            502,
            "auth/state 未返回 state/authUrl，无法发起登录",
        ));
    }
    let started_at = logging::now_ms();
    match flows().lock() {
        Ok(mut table) => {
            // 顺手清掉过期条目（没有后台定时器，取用时扫一遍）
            table.retain(|_, flow| started_at.saturating_sub(flow.started_at) < FLOW_KEEP_MS);
            table.insert(state.clone(), Flow { region, started_at });
        }
        Err(_) => {
            return Err(GatewayError::with_status(
                500,
                "登录流程表不可用（锁中毒），请重启插件后重试",
            ))
        }
    }
    logging::log(
        "[Auth]",
        &format!("发起{}登录，等待浏览器完成…", region.label()),
    );
    Ok(proto::LoginStart {
        flow: state,
        url: auth_url,
        instructions: "在打开的页面里完成登录，完成后本页会自动继续".to_string(),
        poll_interval_ms: POLL_INTERVAL_MS,
        // 授权页在上游域名终结、回调永远不回到本机 —— 由网关轮询收尾
        external_browser: true,
        allowed_hosts: allowed_hosts(),
    })
}

/// 轮询一次：拿到 token 就补齐账号资料并返回 `Done`。
pub(super) async fn login_poll(
    region: Region,
    state: &str,
) -> Result<proto::LoginPoll, GatewayError> {
    // 先确认这条流程归本地区（网关按 provider id 分流，理论上不会错，
    // 但错的那次会让账号落进错地区 —— 症状是转发稳定打错域名，故明确拒绝）
    let known = match flows().lock() {
        Ok(table) => match table.get(state) {
            Some(flow) if flow.region == region => true,
            Some(_) => {
                return Ok(proto::LoginPoll::Failed {
                    message: "这条登录流程属于另一个地区，请重新发起".to_string(),
                })
            }
            None => false,
        },
        Err(_) => false,
    };
    if !known {
        return Ok(proto::LoginPoll::Failed {
            message: "登录流程不存在或已过期，请重新发起".to_string(),
        });
    }

    let context = context_for(region);
    let headers = anonymous_headers();
    let url = context.auth_url(&format!("/auth/token?state={}", encode(state)));
    let token_data = match send("GET", &url, None, &headers).await {
        Ok(response) => match unwrap(&response, "auth/token") {
            Ok(data) => data,
            Err(error) => {
                // 未完成登录时上游回 code=11217；其余错误也只记日志继续等
                // （轮询期间的一次抖动不该打断用户正在进行的授权）
                if error.upstream_code == Some(SERVER_CODES_RETRY_FETCH_TOKEN) {
                    logging::verbose("[Auth]", "等待浏览器登录完成…");
                } else {
                    logging::verbose("[Auth]", &format!("登录轮询中: {}", error.message));
                }
                return Ok(proto::LoginPoll::Pending { hint: None });
            }
        },
        Err(error) => {
            logging::verbose("[Auth]", &format!("登录轮询中: {}", error.message));
            return Ok(proto::LoginPoll::Pending { hint: None });
        }
    };
    let access_token = token_data
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or("");
    if access_token.is_empty() {
        logging::verbose("[Auth]", "等待浏览器登录完成…");
        return Ok(proto::LoginPoll::Pending { hint: None });
    }

    // ── 拿到 token：补资料 → 摘表 → 交账号 ─────────────────────
    if let Ok(mut table) = flows().lock() {
        table.remove(state);
    }
    let secret = build_secret(&token_data, state, &context, region).await;
    let uid = crate::store::pick(&secret, &["uid"]);
    if uid.is_empty() {
        return Ok(proto::LoginPoll::Failed {
            message: "登录成功但获取账号信息失败（缺少 uid），请重试".to_string(),
        });
    }
    logging::log(
        "[Auth]",
        &format!(
            "✅ 登录成功：{}（{}…），已加入账号列表",
            crate::store::pick(&secret, &["name"]),
            crate::json::truncate_text(&uid, 8)
        ),
    );
    Ok(proto::LoginPoll::Done {
        account: proto::NormalizedAccount {
            public: account_view(&secret, region),
            identity: identity_of(&secret),
            secret,
            desktop: false,
        },
    })
}

/// 取消一条流程（用户点了取消，或网关的总超时到了）。
pub(super) fn login_cancel(state: &str) {
    if let Ok(mut table) = flows().lock() {
        table.remove(state);
    }
}

/// 拿到 token 之后拼出**落库形态**的 secret（扁平键，与网关存量记录同形）。
///
/// 身份的三步兜底（后一步只在前一步没给出 uid 时生效）：
///   ① `login/account?state=`（上游刚登录那次最全）
///   ② `/accounts` 列表里「最近登录过的那条」，退而求其次取第一条
///   ③ access token 自己的 JWT 声明（`sub`）
///
/// 第三步是**网络之外**的来源，不能省：前两步都是网络调用，任何一条卡住
/// 就会把整轮登录作废（2026-10-03 实测：国际版授权已成功、token 也拿到了，
/// 只因 `login/account` 连了 30 秒没连上，用户看到的就是「缺少 uid」）。
async fn build_secret(auth: &Value, state: &str, context: &Context, region: Region) -> Value {
    let enriched = with_expires_at(auth.clone());
    let mut account = Value::Object(serde_json::Map::new());
    if !state.is_empty() {
        // 登录时机还没有任何账号上下文，因此出口固定为直连（与网关同口径）
        match fetch_login_account(&enriched, state, context).await {
            Ok(data) if data.is_object() => account = data,
            Ok(_) => {}
            Err(error) => logging::verbose(
                "[Auth]",
                &format!("login/account 拉取失败: {}", error.message),
            ),
        }
    }
    let accounts = match fetch_accounts(&enriched, context).await {
        Ok(list) => list,
        Err(error) => {
            logging::log(
                "[Auth]",
                &format!("拉取账号列表失败（不影响登录）: {}", error.message),
            );
            Vec::new()
        }
    };
    if account.get("uid").and_then(Value::as_str).unwrap_or("").is_empty() {
        let fallback = accounts
            .iter()
            .find(|item| {
                item.get("lastLogin")
                    .map(|value| !value.is_null())
                    .unwrap_or(false)
            })
            .or_else(|| accounts.first())
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        account = fallback;
    }
    if account.get("uid").and_then(Value::as_str).unwrap_or("").is_empty() {
        if let Some(from_token) = account_from_token_claims(&enriched) {
            logging::log(
                "[Auth]",
                "资料接口未给出 uid，已回退用 access token 的 JWT 声明（sub）",
            );
            account = from_token;
        }
    }
    secret_of(&account, &enriched, context, region)
}

/// 把「资料 + 凭证」摊成扁平 secret。
///
/// 键名与网关 `account_store` 的存量记录逐字相同 —— 插件接手老账号时
/// （`flatten_builtin_secret`）字段要能对上，界面列也要能读到。
fn secret_of(account: &Value, auth: &Value, context: &Context, region: Region) -> Value {
    let text = |source: &Value, key: &str| -> String {
        source
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let mut secret = serde_json::Map::new();
    let uid = text(account, "uid");
    secret.insert("uid".to_string(), Value::String(uid.clone()));
    let access_token = text(auth, "accessToken");
    secret.insert("accessToken".to_string(), Value::String(access_token.clone()));
    for key in ["refreshToken", "domain"] {
        let value = text(auth, key);
        if !value.is_empty() {
            secret.insert(key.to_string(), Value::String(value));
        }
    }
    for key in ["expiresAt", "refreshExpiresAt"] {
        if let Some(value) = auth.get(key).filter(|value| !value.is_null()) {
            secret.insert(key.to_string(), value.clone());
        }
    }
    // 展示与组织字段（账号页按这些键渲染）
    let nickname = text(account, "nickname");
    for key in [
        "nickname",
        "type",
        "enterpriseId",
        "enterpriseName",
        "departmentFullName",
    ] {
        let value = text(account, key);
        if !value.is_empty() {
            secret.insert(key.to_string(), Value::String(value));
        }
    }
    // 端点三项：登录时的上下文就是这次账号的落点（staging / 自建网关也走它）
    secret.insert(
        "endpoint".to_string(),
        Value::String(context.base_url.clone()),
    );
    secret.insert(
        "prefixPath".to_string(),
        Value::String(context.prefix.clone()),
    );
    secret.insert(
        "platform".to_string(),
        Value::String(context.platform.clone()),
    );
    secret.insert(
        "edition".to_string(),
        Value::String(region.edition_id().to_string()),
    );
    secret.insert(
        "tokenTail".to_string(),
        Value::String(crate::store::token_tail(&access_token)),
    );
    // 名字：昵称 > `账号 <uid 前 8 位>`（与内置 `add_account` 同款兜底）
    let name = if nickname.is_empty() {
        format!("账号 {}", crate::json::truncate_text(&uid, 8))
    } else {
        nickname
    };
    secret.insert("name".to_string(), Value::String(name));
    Value::Object(secret)
}

/// `GET /login/account?state=`（网关 `AuthService::fetch_login_account` 的插件侧副本）。
async fn fetch_login_account(
    auth: &Value,
    state: &str,
    context: &Context,
) -> Result<Value, GatewayError> {
    let mut headers = bearer_headers(auth, context);
    headers.push(("X-No-User-Id".to_string(), "true".to_string()));
    headers.push(("X-No-Enterprise-Id".to_string(), "true".to_string()));
    headers.push(("X-No-Department-Info".to_string(), "true".to_string()));
    let url = context.auth_url(&format!("/login/account?state={}", encode(state)));
    let response = send("GET", &url, None, &headers).await?;
    unwrap(&response, "login/account")
}

/// `GET /accounts`（网关 `AuthService::fetch_accounts` 的插件侧副本）。
async fn fetch_accounts(auth: &Value, context: &Context) -> Result<Vec<Value>, GatewayError> {
    let headers = bearer_headers(auth, context);
    let url = context.auth_url("/accounts");
    let response = send("GET", &url, None, &headers).await?;
    let data = unwrap(&response, "accounts")?;
    Ok(data
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// 企业扩展头 + Bearer（网关 `AuthService::enterprise_headers` + Authorization）。
fn bearer_headers(auth: &Value, context: &Context) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(enterprise_id) = auth
        .get("account")
        .and_then(|account| account.get("enterpriseId"))
        .and_then(Value::as_str)
    {
        if !enterprise_id.is_empty() {
            headers.push(("X-Enterprise-Id".to_string(), enterprise_id.to_string()));
            headers.push(("X-Tenant-Id".to_string(), enterprise_id.to_string()));
        }
    }
    // 总是带 X-Domain（`auth.domain` 缺省时回退端点 authority）
    let domain = auth
        .get("domain")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| context.authority());
    if !domain.is_empty() {
        headers.push(("X-Domain".to_string(), domain));
    }
    let token = auth.get("accessToken").and_then(Value::as_str).unwrap_or("");
    headers.push(("Authorization".to_string(), format!("Bearer {token}")));
    headers
}

/// 从 access token 的 JWT 声明里取账号身份（`uid` + 昵称）。
///
/// 上游签发的是 Keycloak 风格 JWT，`sub` **就是账号主键**（与库里既有记录的
/// uid 逐字相同，记录 id 形如 `user-<uid>`）。只解 payload、不验签：网关不做
/// 鉴权判定，token 能不能用由上游说了算。
fn account_from_token_claims(auth: &Value) -> Option<Value> {
    let token = auth.get("accessToken").and_then(Value::as_str)?;
    let claims = crate::providers::raccoon::jwt::decode_jwt_claims(token)?;
    let uid = text_of(&claims, &["sub", "userId", "uid"]);
    if uid.is_empty() {
        return None;
    }
    let mut account = serde_json::Map::new();
    account.insert("uid".to_string(), Value::String(uid));
    let name = text_of(
        &claims,
        &["nickname", "name", "preferred_username", "email"],
    );
    if !name.is_empty() {
        account.insert("nickname".to_string(), Value::String(name));
    }
    Some(Value::Object(account))
}

/// 取候选键里第一个非空字符串（去空白）。
fn text_of(object: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(text) = object.get(*key).and_then(Value::as_str) {
            let text = text.trim();
            if !text.is_empty() {
                return text.to_string();
            }
        }
    }
    String::new()
}

/// 补 `expiresAt` / `refreshExpiresAt`（`expiresIn` 秒 → 毫秒时间戳）。
///
/// 与网关 `auth::with_expires_at` 同口径：显式给了就用它，否则按 `expiresIn`
/// 现算；两者都没有时留 0（账号页据此显示「未知」）。
fn with_expires_at(mut auth: Value) -> Value {
    let now = logging::now_ms() as f64;
    let Some(object) = auth.as_object_mut() else {
        return auth;
    };
    let expires_in = object.get("expiresIn").and_then(Value::as_f64).unwrap_or(0.0);
    let refresh_expires_in = object
        .get("refreshExpiresIn")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let expires_at = object
        .get("expiresAt")
        .and_then(Value::as_f64)
        .filter(|value| *value != 0.0)
        .unwrap_or_else(|| if expires_in != 0.0 { now + expires_in * 1000.0 } else { 0.0 });
    let refresh_expires_at = object
        .get("refreshExpiresAt")
        .and_then(Value::as_f64)
        .filter(|value| *value != 0.0)
        .unwrap_or_else(|| {
            if refresh_expires_in != 0.0 {
                now + refresh_expires_in * 1000.0
            } else {
                0.0
            }
        });
    if let Some(object) = auth.as_object_mut() {
        object.insert("expiresAt".to_string(), json!(expires_at));
        object.insert("refreshExpiresAt".to_string(), json!(refresh_expires_at));
    }
    auth
}

// ─── 上下文与传输 ────────────────────────────────────────────

/// 本地区的请求上下文（端点覆盖优先，其次地区默认端点）。
///
/// 覆盖变量按地区分开，理由见 `region::env_endpoint_override`：一个端点只属于
/// 一个站点，共用一个变量名会让「只想改国际版」变成「两地一起改」。
fn context_for(region: Region) -> Context {
    let info = region.edition();
    Context::make(
        region.env_endpoint_override().as_deref(),
        None,
        Some(info.platform),
        Some(info.id),
    )
}

/// 匿名登录请求的头（跳过鉴权的那四个 `X-No-*`）。
fn anonymous_headers() -> Vec<(String, String)> {
    ANONYMOUS_HEADERS
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// 发一次请求（登录时机没有账号上下文，固定直连）。
async fn send(
    method: &str,
    url: &str,
    body: Option<&Value>,
    headers: &[(String, String)],
) -> Result<ApiResponse, GatewayError> {
    send_raw(method, url, body, headers, None, Some(REQUEST_TIMEOUT_MS))
        .await
        .map_err(|error| {
            if error.is_timeout() {
                GatewayError::with_status(504, "登录接口请求超时")
            } else if error.is_connect() {
                GatewayError::with_status(502, format!("无法连接登录接口: {error}"))
            } else {
                GatewayError::with_status(502, format!("登录接口请求失败: {error}"))
            }
        })
}

/// 解包 `{code, data, msg}`（网关 `auth_http::unwrap_response` 的插件侧副本）。
fn unwrap(response: &ApiResponse, api_name: &str) -> Result<Value, GatewayError> {
    let payload = response.payload.as_ref();
    if payload.is_none() && !response.ok {
        return Err(GatewayError::with_status(
            i32::from(response.status),
            format!("{api_name} 返回非 JSON 响应（HTTP {}）", response.status),
        ));
    }
    if !response.ok {
        let message = payload
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .or_else(|| value.get("message").and_then(Value::as_str))
                    .or_else(|| value.get("msg").and_then(Value::as_str))
            })
            .unwrap_or("")
            .to_string();
        let message = if message.is_empty() {
            format!("HTTP {}", response.status)
        } else {
            message
        };
        let code = payload
            .and_then(|value| value.get("code"))
            .and_then(Value::as_i64);
        return Err(
            GatewayError::with_status(i32::from(response.status), format!("{api_name} 失败: {message}"))
                .with_optional_code(code),
        );
    }
    if let Some(Value::Number(code)) = payload.and_then(|value| value.get("code")) {
        if let Some(code) = code.as_i64() {
            if code != RESPONSE_CODE_OK {
                let detail = payload
                    .and_then(|value| value.get("message").and_then(Value::as_str))
                    .or_else(|| payload.and_then(|value| value.get("msg").and_then(Value::as_str)))
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let message = if detail.is_empty() {
                    format!("{api_name} 失败: code={code}")
                } else {
                    format!("{api_name} 失败: code={code} {detail}")
                };
                return Err(GatewayError::with_status(502, message).upstream_code(code));
            }
        }
    }
    Ok(payload
        .and_then(|value| value.get("data"))
        .cloned()
        .unwrap_or(Value::Null))
}

/// 查询串的百分号编码（与网关 `auth::urlencoding` 同一套规则）。
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric()
            || matches!(ch, '-' | '_' | '.' | '!' | '~' | '*' | '\'' | '(' | ')')
        {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 登录窗口的导航白名单（网关桌面壳 `WORKBUDDY_ALLOWED_HOSTS` 的同义物）。
///
/// 两家共用同一张表：表里 `workbuddy.ai` 那一行就是国际站的登录域。
/// 空列表会被网关解释成「不限制」，所以这里必须给全 —— 少一行的症状是
/// 「登录窗口白屏」，而日志上什么也看不出。
fn allowed_hosts() -> Vec<String> {
    [
        "copilot.tencent.com",
        "staging-copilot.tencent.com",
        "codebuddy.cn",
        "workbuddy.cn",
        "codebuddy.ai",
        "staging-codebuddy.tencent.com",
        "workbuddy.ai",
        "staging.workbuddy.ai",
        "tencent.com",
        "qq.com",
        "wechat.com",
        "weixin.qq.com",
        "tenpay.com",
    ]
    .iter()
    .map(|host| host.to_string())
    .collect()
}
