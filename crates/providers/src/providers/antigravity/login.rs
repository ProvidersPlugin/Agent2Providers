//! Antigravity 登录：**粘贴式校验** + **网页登录（Google OAuth 授权码）**。
//!
//! ── 粘贴式（`verify_paste`）──────────────────────────────────
//! 归一化 → 一次真刷新校验 → best-effort 补 project / email。归一化认得的形态：
//! ```text
//!   1//0gxxxx…                      裸 refresh token（1// 是令牌本体，必须留着）
//!   "1//0gxxxx…"                    带成对引号
//!   refresh_token=1//0gxxxx…        表单 / 环境变量整行
//!   "refresh_token": "1//0gxxxx…"   JSON 片段（DevTools 复制）
//!   Bearer ya29.xxxx                access token 整行（剥 Bearer）
//! ```
//! 判据是**扫描 `1//` 出现的位置**：从那儿取到行尾，再削掉引号 / 尾逗号 / 尾花
//! 括号 / 空白。扫不到 `1//` 时只剥引号与空白，把整串当令牌（Google 的 refresh
//! token 一定有 `1//`，但**不因为形态不符就拒绝**：上游会用 401 明确告诉用户
//! 串不对，而形态猜错会把一个能用的串挡在门外）。
//!
//! ── 网页登录为什么自绑 loopback 端口 ────────────────────────
//! 网关那版把回调挂在**网关自己的端口** `/oauth-callback` 上，靠 `state` 认回
//! 那一轮。插件侧改成**一轮一个随机端口**（`callback_server`）：
//!   - 插件不知道网关的端口策略（`HostEnv.loopback_port` 只给一个数），而
//!     插件自绑端口不需要网关配合，也不再依赖「网关那条路由还在」；
//!   - 端口天然表达「这一轮是谁的」，`state` 仍然逐字比对（两道闸门）。
//! 因为回调落在**插件自己**的 loopback 端口上，永远不经过网关的
//! `/api/session/login/callback` —— 所以 `poll_interval_ms` **必须 > 0**
//! （「轮询型」在协议里的含义就是「网关主动问插件要结果」，与授权在哪里终结
//! 无关）。报 `0` 的话网关会一直挂着等一个不会到来的回调，没有任何报错。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};

use agent2api_provider_protocol as proto;
use agent2api_provider_protocol::WireProxy as ResolvedProxy;

use crate::errors::GatewayError;
use crate::logging;

use super::callback_server::CallbackListener;
use super::credentials::REFRESH_TOKEN_PREFIX;
use super::{accounts, oauth, project};

/// 输入长度上限（超过这个长度的输入一定是粘错了东西）
pub const MAX_INPUT_LENGTH: usize = 8192;

/// 流程 id 前缀
const FLOW_PREFIX: &str = "antigravity-web-";

/// 一轮登录在进程内的状态。
enum FlowState {
    /// 授权地址已给出，等回调。
    Waiting,
    /// 拿到授权码，正在换凭证。
    Working,
    /// 换好了（存的是凭证的落库形态）。
    Done(Value),
    /// 这一轮没成。
    Failed(String),
}

struct Entry {
    listener: Arc<CallbackListener>,
    flow_state: FlowState,
}

fn flows() -> &'static Mutex<HashMap<String, Entry>> {
    static FLOWS: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    FLOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_flows<T>(action: impl FnOnce(&mut HashMap<String, Entry>) -> T) -> T {
    match flows().lock() {
        Ok(mut table) => action(&mut table),
        Err(poisoned) => action(&mut poisoned.into_inner()),
    }
}

// ─── 粘贴式 ─────────────────────────────────────────────────

/// 去掉成对的引号与两侧的分隔符（JSON / 表单复制常见的包装字符）
fn trim_wrappers(text: &str) -> String {
    text.trim()
        .trim_matches(|ch| matches!(ch, '"' | '\'' | '`' | ',' | ';'))
        .trim()
        .to_string()
}

/// 剥掉整行前缀：`refresh_token=` / `refreshToken:` / `access_token=` 之类。
fn strip_label(text: &str) -> String {
    const LABELS: [&str; 6] = [
        "refresh_token",
        "refreshtoken",
        "refresh token",
        "access_token",
        "accesstoken",
        "token",
    ];
    let lowered = text.to_ascii_lowercase();
    for label in LABELS {
        if !lowered.starts_with(label) {
            continue;
        }
        let rest = text.get(label.len()..).unwrap_or("").trim_start();
        let Some(rest) = rest
            .strip_prefix('=')
            .or_else(|| rest.strip_prefix(':'))
            .or_else(|| rest.strip_prefix("=>"))
        else {
            continue;
        };
        return trim_wrappers(rest);
    }
    text.to_string()
}

/// 归一化粘贴的 refresh token：剥掉整行包装与引号，返回**裸令牌**。
///
/// `1//` 前缀是令牌本体的一部分，**绝不要剥**（见模块头）。
pub fn normalize_refresh_token(raw: &str) -> String {
    let cleaned = strip_label(&trim_wrappers(raw));
    if let Some(start) = cleaned.find(REFRESH_TOKEN_PREFIX) {
        let tail = cleaned.get(start..).unwrap_or("");
        let token: String = tail
            .chars()
            .take_while(|ch| {
                !ch.is_whitespace() && !matches!(ch, '"' | '\'' | ',' | '}' | ']' | ';')
            })
            .collect();
        if token.len() > REFRESH_TOKEN_PREFIX.len() {
            return token;
        }
    }
    // 扫不到 `1//`：只把空白去掉（不是合法形态也交给上游 401 说话）
    cleaned.chars().filter(|ch| !ch.is_whitespace()).collect()
}

/// 归一化粘贴的 access token：剥 `Bearer ` 前缀、引号与空白。
pub fn normalize_access_token(raw: &str) -> String {
    let cleaned = strip_label(&trim_wrappers(raw));
    let trimmed = cleaned.trim();
    // `get` 而不是切片：非 ASCII 首字符时不做越界的字符边界切片
    let has_bearer = trimmed
        .get(..6)
        .is_some_and(|head| head.eq_ignore_ascii_case("bearer"));
    let without_bearer = if has_bearer {
        trimmed.get(6..).unwrap_or("").trim()
    } else {
        trimmed
    };
    without_bearer
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect()
}

/// 从 payload 里取第一个非空字符串（界面的表单键名与落盘名都认）
fn pick(payload: &Value, keys: &[&str]) -> String {
    for key in keys {
        let value = payload
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !value.is_empty() {
            return value.to_string();
        }
    }
    String::new()
}

/// 粘贴式添加的入口：归一化 → 校验刷新（真打 Google）→ best-effort 补 project/email。
///
/// ── project / email 是 best-effort ───────────────────────────
/// 两者都不影响「账号能不能用」：project 只影响聊天请求（规格 §3.4），
/// email 只影响展示。拿不到就留空，**绝不因为它失败就判登录失败**。
pub(super) async fn verify_paste(
    payload: &Value,
    proxy: Option<&ResolvedProxy>,
) -> Result<super::credentials::AntigravityCredentials, GatewayError> {
    let raw_refresh = pick(payload, &["refreshToken", "refresh_token", "token"]);
    if raw_refresh.trim().is_empty() {
        return Err(GatewayError::with_status(
            400,
            "请粘贴 Antigravity 的 Google refresh token（以 1// 开头）",
        ));
    }
    if raw_refresh.chars().count() > MAX_INPUT_LENGTH {
        return Err(GatewayError::with_status(
            400,
            "粘贴的内容过长，请确认复制的是 refresh token 本身",
        ));
    }
    let refresh_token = normalize_refresh_token(&raw_refresh);
    if refresh_token.is_empty() {
        return Err(GatewayError::with_status(
            400,
            "请粘贴 Antigravity 的 Google refresh token（以 1// 开头）",
        ));
    }
    // 校验刷新：成功即证明串可用（失败时 oauth 模块给出可照做的文案）
    let response = oauth::refresh_access_token(&refresh_token, proxy).await?;
    let access_token = normalize_access_token(&response.access_token);
    let expires_in = if (1..=7 * 24 * 3600).contains(&response.expires_in) {
        response.expires_in
    } else {
        3600
    };
    let expires_at = logging::now_ms() + expires_in * 1000;
    // 表单里手填的 project 优先（用户可能自己知道）；没有再 best-effort 发现
    let provided_project = pick(payload, &["projectId", "project_id"]);
    let mut project_id = provided_project.trim().to_string();
    if project_id.is_empty() {
        match project::discover_project(&access_token, proxy).await {
            Ok(found) => project_id = found,
            Err(error) => logging::verbose(
                "[Login]",
                &format!(
                    "Antigravity project 未发现（不影响账号添加）：{}",
                    error.message
                ),
            ),
        }
    }
    let email = pick(payload, &["email"]);
    logging::log(
        "[Login]",
        &format!(
            "✅ Antigravity refresh token 校验通过（{}{}）",
            if email.is_empty() {
                "email 未填".to_string()
            } else {
                format!("email {email}")
            },
            if project_id.is_empty() {
                "，project 未发现（刷模型清单时会再试）".to_string()
            } else {
                format!("，project {project_id}")
            }
        ),
    );
    Ok(super::credentials::AntigravityCredentials {
        id: String::new(),
        refresh_token,
        access_token,
        expires_at,
        project_id,
        email,
        name: String::new(),
    })
}

// ─── 网页登录 ───────────────────────────────────────────────

/// 发起一次网页登录（或对非 browser 方法给出可照做的拒绝）。
pub(super) async fn login_start(
    method: &str,
    payload: &Value,
) -> Result<proto::LoginStart, String> {
    if method != "browser" {
        return Err(
            "Antigravity 的「填写凭据」请直接在账号页粘贴 Google refresh token；网页登录请点「网页登录」"
                .to_string(),
        );
    }
    // 这一轮登录的出网线路：网关在 `login.start` 的 payload 里给（用户选的
    // 代理，已解析成 WireProxy）。解析失败要明确拒绝 —— 静默回落直连会让
    // 无头服务器上的登录挂在「连不上 Google」，而用户以为自己选了代理。
    let proxy = resolve_login_proxy(payload.get("proxy"))?;
    // 上一轮没收摊的先关掉：端口不释放的话，这一轮的回调可能落到上一轮的
    // 监听器上（授权码一次性，两轮并存等于让其中一轮必输）。
    close_all();
    let listener = Arc::new(CallbackListener::bind().await?);
    let redirect_uri = format!(
        "http://127.0.0.1:{}{}",
        listener.port(),
        super::callback_server::PATH
    );
    let state = new_state()?;
    let auth_url = oauth::build_authorize_url(&redirect_uri, &state);
    if auth_url.is_empty() {
        listener.close();
        return Err("没能生成 Google 授权地址（内置的授权端点常量有误）".to_string());
    }
    let flow_id = format!("{FLOW_PREFIX}{state}");
    with_flows(|table| {
        table.insert(
            flow_id.clone(),
            Entry {
                listener: Arc::clone(&listener),
                flow_state: FlowState::Waiting,
            },
        );
    });
    spawn_completion(
        flow_id.clone(),
        Arc::clone(&listener),
        state,
        redirect_uri,
        proxy,
    );
    logging::log(
        "[Login]",
        &format!(
            "发起 Antigravity 网页登录（loopback 127.0.0.1:{}，等待浏览器回调…）",
            listener.port()
        ),
    );
    Ok(proto::LoginStart {
        flow: flow_id,
        url: auth_url,
        instructions: "在打开的页面里完成 Google 账号授权，完成后会自动收下账号".to_string(),
        // 见模块头：回调打在插件自己的 loopback 端口上，网关必须来问
        poll_interval_ms: 2_000,
        // 内嵌窗口：回调是 `http://127.0.0.1:<port>/oauth-callback`，
        // 同一台机器上内嵌窗口能正常导航过去
        external_browser: false,
        // 空 = 不限制：授权页会继续跳 Google 的多个主机
        allowed_hosts: Vec::new(),
    })
}

/// 解析 `login.start` 里的登录代理（`payload.proxy`）。
///
/// ── 三种输入的语义 ──────────────────────────────────────────
///   - 缺省 / `null` / `source: "direct"` → `None`：没选代理，跟随系统代理
///     （`client_for(None)` 的老口径，桌面端照旧）；
///   - 可解析的代理 → `Some(WireProxy)`：登录链路全程走它；
///   - 给了但解析不了 → 明确报错。**不静默回落直连**：无头服务器上「连不上
///     Google」与「选了代理但代理坏了」是两件事，后者用户能自己修。
fn resolve_login_proxy(raw: Option<&Value>) -> Result<Option<ResolvedProxy>, String> {
    let Some(raw) = raw else { return Ok(None) };
    if raw.is_null() {
        return Ok(None);
    }
    // 网关的 `checked_login_proxy` 既可能给解析后的 `WireProxy`，也可能给
    // 原始配置（`source` / `proxyId` 形态）。先按 WireProxy 认，认不出再走
    // 账号级代理的解析器（它对 `direct` / `rotate` / 池引用都有明确结论）。
    if let Ok(proxy) = serde_json::from_value::<ResolvedProxy>(raw.clone()) {
        if !proxy.host.trim().is_empty() {
            return Ok(Some(proxy));
        }
    }
    match crate::proxy_util::resolve_account_proxy(Some(raw)) {
        None => Ok(None),
        Some(crate::proxy_util::ProxyResolution::Resolved(proxy)) => Ok(Some(proxy)),
        Some(crate::proxy_util::ProxyResolution::Failed(reason)) => Err(format!(
            "这一轮登录的代理不可用（{reason}）：请在登录弹窗里换一条代理，或先修好它再重试"
        )),
    }
}

/// 后台那半个登录：等回调 → 换码 → 写状态。
fn spawn_completion(
    flow_id: String,
    listener: Arc<CallbackListener>,
    state: String,
    redirect_uri: String,
    proxy: Option<ResolvedProxy>,
) {
    tokio::spawn(async move {
        let Some(query) = listener.next_callback().await else {
            // 通道关了（用户取消 / 面板超时清理）—— 把状态如实标出来
            with_flows(|table| {
                if let Some(entry) = table.get_mut(&flow_id) {
                    if matches!(entry.flow_state, FlowState::Waiting) {
                        entry.flow_state =
                            FlowState::Failed("这次登录已取消，请重新发起".to_string());
                    }
                }
            });
            return;
        };
        set_state(&flow_id, FlowState::Working);
        let outcome = finish(&query, &state, &redirect_uri, proxy.as_ref()).await;
        with_flows(|table| {
            if let Some(entry) = table.get_mut(&flow_id) {
                entry.flow_state = match outcome {
                    Ok(credentials) => FlowState::Done(credentials_to_value(&credentials)),
                    Err(message) => FlowState::Failed(message),
                };
            }
        });
        // 这一轮收摊：端口还回去
        listener.close();
    });
}

fn set_state(flow_id: &str, next: FlowState) {
    with_flows(|table| {
        if let Some(entry) = table.get_mut(flow_id) {
            entry.flow_state = next;
        }
    });
}

/// 用回调 query 收尾：校验 state → 换码。
async fn finish(
    query: &str,
    expected_state: &str,
    redirect_uri: &str,
    proxy: Option<&ResolvedProxy>,
) -> Result<super::credentials::AntigravityCredentials, String> {
    let params = parse_query(query);
    let get = |key: &str| {
        params
            .get(key)
            .map(|value| percent_decode(value))
            .unwrap_or_default()
    };
    if let Some(error) = params.get("error").map(|value| percent_decode(value)) {
        if !error.trim().is_empty() {
            return Err(format!("Google 授权被拒绝（{error}）"));
        }
    }
    let callback_state = get("state");
    if callback_state.is_empty() {
        return Err("回调里缺少 state，无法确认这次登录由本机发起，请重新发起".to_string());
    }
    if callback_state != expected_state {
        return Err("回调里的 state 与本次登录不一致，已拒绝".to_string());
    }
    let code = get("code");
    if code.trim().is_empty() {
        return Err("回调里没有授权码：请重新发起网页登录".to_string());
    }
    oauth::exchange_code(&code, &callback_state, redirect_uri, proxy)
        .await
        .map_err(|error| error.message)
}

/// 轮询一次登录进度。
pub(super) async fn login_poll(flow: &str) -> Result<proto::LoginPoll, String> {
    let state = with_flows(|table| match table.get(flow) {
        Some(entry) => Some(match &entry.flow_state {
            FlowState::Waiting => proto::LoginPoll::Pending {
                hint: Some("等待你在浏览器里完成 Google 授权…".to_string()),
            },
            FlowState::Working => proto::LoginPoll::Pending {
                hint: Some("已收到授权码，正在换取凭证…".to_string()),
            },
            FlowState::Done(payload) => done_poll(payload),
            FlowState::Failed(message) => proto::LoginPoll::Failed {
                message: message.clone(),
            },
        }),
        None => None,
    });
    match state {
        Some(result) => {
            if is_terminal(&result) {
                with_flows(|table| table.remove(flow));
            }
            Ok(result)
        }
        None => Ok(proto::LoginPoll::Failed {
            message: "这次登录流程已失效（插件进程重启过？），请重新发起".to_string(),
        }),
    }
}

/// 用户粘贴回来的回调 URL。
///
/// ── 这条为什么存在 ──────────────────────────────────────────
/// 授权页在本机跑时回调能直达 loopback；但如果用户是在**另一台机器**上完成
/// 授权（远程浏览器 / 容器里跑网关），回调就落不到这一台上 —— 那时用户会
/// 把浏览器地址栏里那条 URL 粘回来。两条路共用同一个监听器（投递后由后台
/// 任务照常换证），不复制第二套换证逻辑。
pub(super) async fn login_callback(
    callback: proto::LoginCallback,
) -> Result<proto::LoginPoll, String> {
    let query = callback_query(&callback);
    let listener = with_flows(|table| {
        table.get(&callback.flow).map(|entry| {
            entry.listener.submit_callback(&query)?;
            Ok::<_, String>(Arc::clone(&entry.listener))
        })
    });
    let Some(listener) = listener else {
        return Ok(proto::LoginPoll::Failed {
            message: "这次登录流程已失效，请重新发起".to_string(),
        });
    };
    let _listener = listener?;
    // 后台任务已经在等回调，投递之后它会立刻进入换证。这里只等状态翻转，
    // 不自己再消费一次 channel（两个 consumer 抢同一个 receiver 只会让
    // 其中一边永远等不到）。
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let state = with_flows(|table| match table.get(&callback.flow) {
            Some(entry) => match &entry.flow_state {
                FlowState::Done(payload) => Some(done_poll(payload)),
                FlowState::Failed(message) => Some(proto::LoginPoll::Failed {
                    message: message.clone(),
                }),
                _ => None,
            },
            None => Some(proto::LoginPoll::Failed {
                message: "这次登录流程已失效，请重新发起".to_string(),
            }),
        });
        if let Some(result) = state {
            if is_terminal(&result) {
                with_flows(|table| table.remove(&callback.flow));
            }
            return Ok(result);
        }
    }
    // 30 秒还没换完：如实说「还在换」
    Ok(proto::LoginPoll::Pending {
        hint: Some("已收到授权信息，仍在换取凭证…".to_string()),
    })
}

/// 取消一轮登录：收监听器（释放端口）并清掉那一条。
pub(super) async fn login_cancel(flow: &str) -> Result<(), String> {
    let removed = with_flows(|table| table.remove(flow));
    if let Some(entry) = removed {
        entry.listener.close();
    }
    Ok(())
}

/// 收掉表里所有轮次（发起新登录前调）。
fn close_all() {
    let drained: Vec<Entry> = with_flows(|table| table.drain().map(|(_, entry)| entry).collect());
    for entry in drained {
        entry.listener.close();
    }
}

/// 一轮登录的 state：32 字节随机 → 64 位十六进制（不可预测，满足 CSRF 要求）。
fn new_state() -> Result<String, String> {
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|error| format!("随机数不可用：{error}"))?;
    Ok(seed.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// 凭证 → 落库形态（存进 `FlowState::Done`，`done_poll` 再读回）。
fn credentials_to_value(credentials: &super::credentials::AntigravityCredentials) -> Value {
    json!({
        "refreshToken": credentials.refresh_token,
        "accessToken": credentials.access_token,
        "expiresAt": credentials.expires_at,
        "projectId": credentials.project_id,
        "email": credentials.email,
    })
}

/// 换证结果 → `LoginPoll`（归一化在 `accounts`，失败翻成 `Failed`）。
fn done_poll(payload: &Value) -> proto::LoginPoll {
    let credentials = super::credentials::AntigravityCredentials {
        id: String::new(),
        refresh_token: crate::store::pick(payload, &["refreshToken"]),
        access_token: crate::store::pick(payload, &["accessToken"]),
        expires_at: payload.get("expiresAt").and_then(Value::as_i64).unwrap_or(0),
        project_id: crate::store::pick(payload, &["projectId"]),
        email: crate::store::pick(payload, &["email"]),
        name: String::new(),
    };
    if credentials.refresh_token.is_empty() {
        return proto::LoginPoll::Failed {
            message: "这一轮登录没有拿到 refresh token，请重新发起网页登录".to_string(),
        };
    }
    proto::LoginPoll::Done {
        account: accounts::account_of(&credentials, "web-login", None),
    }
}

fn is_terminal(poll: &proto::LoginPoll) -> bool {
    matches!(
        poll,
        proto::LoginPoll::Done { .. } | proto::LoginPoll::Failed { .. }
    )
}

/// `LoginCallback` → 回调 query 串（带 `?` 时取问号后）。
fn callback_query(callback: &proto::LoginCallback) -> String {
    let url = callback.url.trim();
    if let Some((_, query)) = url.split_once('?') {
        return query.to_string();
    }
    if url.contains('=') {
        return url.to_string();
    }
    // 只给了 code：拼成最小的 query（`finish` 认得）
    if callback.code.trim().is_empty() {
        String::new()
    } else {
        format!("code={}", callback.code.trim())
    }
}

/// 查询串 → 键值对（**不做**解码：`finish` 里按需解）。
fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// `application/x-www-form-urlencoded` 的解码（`+` = 空格，`%XX` = 字节）。
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = &value[index + 1..index + 3];
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
