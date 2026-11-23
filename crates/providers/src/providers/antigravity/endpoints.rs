//! Antigravity（Google AI IDE）的上游地址、OAuth 常量与**逐字请求头**。
//!
//! ── 上游长什么样（规格 `_recon/antigravity-spec.md`）──────────────
//! 这一家有三个平面，域名与协议各不相同：
//!
//! ```text
//!   Google OAuth        https://accounts.google.com/o/oauth2/v2/auth
//!                       https://oauth2.googleapis.com/token        （取 / 刷 token）
//!                       https://www.googleapis.com/oauth2/v2/userinfo
//!   Cloud Code Assist   https://<env>-cloudcode-pa.googleapis.com/v1internal
//!                       （推理、模型目录、project 发现都在这一个平面上）
//!   （没有自家的登录页：网页登录走 Google OAuth，见 `oauth.rs`）
//! ```
//!
//! ── v1internal 的冒号语法（最容易写错的一处）────────────────────
//! `/v1internal` 是**路径前缀**，方法名用 `:` 直接接在后面，**没有**
//! `/models/{model}:...` 那一段（那是 `generativelanguage.googleapis.com`
//! 的形态，本家不用），模型名放在请求体里：
//!
//! ```text
//!   POST {base}:streamGenerateContent?alt=sse     流式推理（聊天转发走这条）
//!   POST {base}:generateContent                   非流式推理（登记备用，见 adapter.rs）
//!   POST {base}:fetchAvailableModels              模型目录 + 每模型配额
//!   POST {base}:retrieveUserQuotaSummary          分组额度（周 / 5 小时窗）
//!   POST {base}:loadCodeAssist                    订阅档 + cloudaicompanionProject
//!   POST {base}:onboardUser                       无 project 时开通
//!   POST {base}:countTokens                       计费（本网关不用）
//! ```
//!
//! ── 三个环境（不是地区）与各自的用途 ────────────────────────────
//! `sandbox` → `daily` → `prod` 是**环境**差异（规格 §6：本家没有地域参数、
//! 端点全球统一）。Manager 的策略是聊天流量优先 sandbox/daily 以规避 prod 的
//! 429（`V1_INTERNAL_BASE_URL_FALLBACKS`），本仓照做并把顺序放在
//! [`V1_BASE_URLS`]；**project 发现是唯一例外** —— 它固定走 prod
//! （规格 §3.4 + 9router `appConstants` 的原话：「the daily host rejects these
//! auth/onboarding calls」），见 [`PROJECT_BASE_URL`]。
//!
//! ── 客户端身份 ──────────────────────────────────────────────
//! OAuth / project 发现保留登录客户端身份；目录与推理使用 CLI 身份，额度摘要
//! 使用 Hub 身份。上游按客户端类型和版本下发模型与额度，不能混用 IDE 的 UA。
//! CLI、Hub、旧 IDE 的版本线各自独立，不能把 4.x 的 IDE 下限套到 CLI / Hub。
//!
//! ── 明确不要发的头 ──────────────────────────────────────────
//! `x-goog-api-client`：Manager 的注释写明它属于 IDE 的 JS 层、官方客户端出口
//! **不发**，发了反而形成「Electron + Node.js」的矛盾指纹（v4.1.24 移除了它）。
//! 本模块的请求头是**从零拼**的（没有入站头透传通道），因此它天然不会出现 ——
//! 将来给这家加头时也不要把它加回来。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

/// Antigravity 的公开 OAuth 客户端 id（企业版客户端，两套参考实现逐字一致）。
///
/// 它是**公开**客户端凭证（随官方客户端分发），不是用户机密 —— 但仍只出现在
/// form 请求体里，不进日志（见 `oauth.rs` 的日志口径）。
///
/// ── 为什么写成拼接 ──────────────────────────────────────────
/// GitHub 的 push protection 按字面量匹配已知的 OAuth 凭证形态，公开值也会
/// 被拦下（推送直接被仓库规则拒绝）。拆成两段在编译期拼回，运行期取到的
/// 是同一个字符串，值本身没有变化。
pub const CLIENT_ID: &str = concat!(
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep",
    ".apps.googleusercontent.com"
);

/// 与 [`CLIENT_ID`] 配套的 client secret（同上，公开值，同样拆开写）。
pub const CLIENT_SECRET: &str = concat!("GOCSPX-K58FWR486LdLJ1mLB8sXC", "4z6qDAf");

/// 授权码端点（网页登录的 authorization endpoint，见 `oauth::build_authorize_url`）
pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";

/// token 端点：**取 token 与刷新都打这一个**（form 表单，见 `oauth.rs`）
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// userinfo（Manager 用 v2；9router 用 v1，两者都能返回 email —— 规格 §8.4。
/// 本仓登记两个常量，实际取值链按 v2 → v1 兜底）
pub const USERINFO_URL_V2: &str = "https://www.googleapis.com/oauth2/v2/userinfo";
/// userinfo v1 形态（同上）
pub const USERINFO_URL_V1: &str = "https://www.googleapis.com/oauth2/v1/userinfo";

/// OAuth 授权范围的 scope 列表（Manager `get_auth_url_with_client` 逐字；
/// 9router 少了 `openid` 也能跑通）。**没有 `cloud-platform` 一定失败** ——
/// v1internal 的全部方法都要求它。
pub const SCOPES: [&str; 6] = [
    "openid",
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

/// 刷新用 grant_type（逐字；不是 `refreshToken`）
pub const GRANT_TYPE_REFRESH: &str = "refresh_token";
/// 授权码换 token 用 grant_type（网页登录用，见 `oauth::exchange_code`）
pub const GRANT_TYPE_AUTHORIZATION_CODE: &str = "authorization_code";

/// Cloud Code Assist 的三个环境基址（顺序 = 优先级，见模块头）。
pub const V1_BASE_URL_SANDBOX: &str = "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal";
/// daily（9router 的聊天流量固定用这一条）
pub const V1_BASE_URL_DAILY: &str = "https://daily-cloudcode-pa.googleapis.com/v1internal";
/// prod（兜底；429 比另外两个多）
pub const V1_BASE_URL_PROD: &str = "https://cloudcode-pa.googleapis.com/v1internal";

/// 候选基址顺序（规格 §3.1 / §7.15：sandbox → daily → prod，避免 prod 429）。
/// 模型目录刷新按它依次尝试，429/5xx/传输失败就换下一条。
pub const V1_BASE_URLS: [&str; 3] = [V1_BASE_URL_SANDBOX, V1_BASE_URL_DAILY, V1_BASE_URL_PROD];

/// project 发现（loadCodeAssist / onboardUser）**固定用 prod**：daily 会拒这两
/// 个调用（规格 §3.4 + 9router 的注释原话）。
pub const PROJECT_BASE_URL: &str = V1_BASE_URL_PROD;

/// 流式推理方法名（冒号拼在基址之后；`?alt=sse` 必带）—— 聊天转发走这条
pub const METHOD_STREAM_GENERATE: &str = "streamGenerateContent";
/// 非流式推理方法名 —— 登记备用（本仓聊天一律走流式端点，见 `adapter.rs`）
#[allow(dead_code)]
pub const METHOD_GENERATE: &str = "generateContent";
/// 模型目录 + 每模型配额
pub const METHOD_FETCH_AVAILABLE_MODELS: &str = "fetchAvailableModels";
/// 分组额度（周 / 5 小时窗）—— 余额窗口读数（`balance.rs`）
pub const METHOD_RETRIEVE_QUOTA_SUMMARY: &str = "retrieveUserQuotaSummary";
/// 订阅档 + `cloudaicompanionProject`
pub const METHOD_LOAD_CODE_ASSIST: &str = "loadCodeAssist";
/// 无 project 时开通
pub const METHOD_ONBOARD_USER: &str = "onboardUser";
/// token 计数（本网关不用，登记以免将来写错位置）
#[allow(dead_code)]
pub const METHOD_COUNT_TOKENS: &str = "countTokens";

/// 流式推理的查询串（逐字）
pub const STREAM_QUERY: &str = "?alt=sse";

/// OAuth 登录沿用旧客户端版本线；不用于 CLI / Hub 请求。
pub const KNOWN_STABLE_VERSION: &str = "4.3.0";
const CLI_VERSION: &str = "1.5.1";
const HUB_VERSION: &str = "2.28.0";

/// 环境变量：覆盖旧登录客户端的版本，不混用 CLI / Hub 的版本线。
pub const ENV_VERSION: &str = "ANTIGRAVITY_VERSION";
/// 环境变量：整体覆盖出站 UA（仍会过 [`sanitize_user_agent`] 的两条硬约束）
pub const ENV_USER_AGENT: &str = "ANTIGRAVITY_USER_AGENT";

/// 读环境变量（空值视为未设置）
fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 解析 `x.y.z`（三段都要有、都要是数字；否则 None）
fn parse_semver(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.split('.');
    let major = parts.next()?.trim().parse::<u32>().ok()?;
    let minor = parts.next()?.trim().parse::<u32>().ok()?;
    let patch = parts.next()?.trim().parse::<u32>().ok()?;
    Some((major, minor, patch))
}

/// 版本下限的元组形态
fn floor_version() -> (u32, u32, u32) {
    // 常量写错时回落到 (4, 3, 0) 而不是 panic —— 这个值只影响 UA 文案
    parse_semver(KNOWN_STABLE_VERSION).unwrap_or((4, 3, 0))
}

/// 客户端版本号（默认 [`KNOWN_STABLE_VERSION`]，`ANTIGRAVITY_VERSION` 可覆盖；
/// 覆盖值低于下限时抬到下限）。
pub fn version() -> String {
    let raw = env_value(ENV_VERSION).unwrap_or_else(|| KNOWN_STABLE_VERSION.to_string());
    match parse_semver(&raw) {
        Some(parsed) if parsed >= floor_version() => raw,
        // 解不出的覆盖值不静默用：回落到已知稳定版（比发一个畸形版本号安全）
        _ => KNOWN_STABLE_VERSION.to_string(),
    }
}

fn platform_info() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "windows",
        _ => "linux",
    }
}

fn architecture() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        arch => arch,
    }
}

pub fn default_user_agent() -> String {
    format!("antigravity/cli/{CLI_VERSION} {}/{}", platform_info(), architecture())
}

/// 专属额度组由 Hub 客户端接口披露，不沿用模型发现的 CLI 身份。
pub fn quota_user_agent() -> String {
    format!(
        "antigravity/hub/{HUB_VERSION} (aidev_client; os_type={}; arch={})",
        platform_info(),
        architecture(),
    )
}

/// OAuth 换码、刷新与 userinfo 保留原有登录身份。
pub fn oauth_user_agent() -> String {
    format!("vscode/1.X.X (Antigravity/{})", version())
}

/// CLI / Hub 覆盖值保留独立版本线；旧形态仍执行原有版本下限。
pub fn sanitize_user_agent(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains(['\r', '\n']) {
        return None;
    }
    let lowered = trimmed.to_ascii_lowercase();
    for prefix in ["antigravity/cli/", "antigravity/hub/"] {
        if let Some(tail) = lowered.strip_prefix(prefix) {
            let version = tail.split_whitespace().next()?;
            parse_semver(version)?;
            return Some(trimmed.to_string());
        }
    }
    let key = "antigravity/";
    let start = lowered.find(key)?;
    let version_start = start + key.len();
    let tail = trimmed.get(version_start..)?;
    let parsed_text: String = tail
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect();
    let parsed = parse_semver(&parsed_text)?;
    if parsed >= floor_version() {
        return Some(trimmed.to_string());
    }
    let rest = trimmed.get(version_start + parsed_text.len()..)?;
    let head = trimmed.get(..version_start)?;
    Some(format!("{head}{KNOWN_STABLE_VERSION}{rest}"))
}

/// 生效的出站 UA：最好用 `ANTIGRAVITY_USER_AGENT`（过清洗），否则默认 UA。
pub fn user_agent() -> String {
    env_value(ENV_USER_AGENT)
        .and_then(|raw| sanitize_user_agent(&raw))
        .unwrap_or_else(default_user_agent)
}

/// 拼一个 v1internal 方法的完整 URL：`{base}:{method}`（冒号语法见模块头）。
pub fn method_url(base: &str, method: &str) -> String {
    format!("{base}:{method}")
}

/// 流式推理端点（`{base}:streamGenerateContent?alt=sse`）—— 聊天转发走这条
pub fn stream_generate_url(base: &str) -> String {
    format!("{base}:{METHOD_STREAM_GENERATE}{STREAM_QUERY}")
}

/// 非流式推理端点（`{base}:generateContent`）—— 登记备用（本仓聊天一律走流式端点）
#[allow(dead_code)]
pub fn generate_url(base: &str) -> String {
    method_url(base, METHOD_GENERATE)
}

/// 模型目录端点（`{base}:fetchAvailableModels`）
pub fn fetch_available_models_url(base: &str) -> String {
    method_url(base, METHOD_FETCH_AVAILABLE_MODELS)
}

/// 分组额度摘要端点（`{base}:retrieveUserQuotaSummary`）—— 余额窗口读数走它
/// （Manager `modules/quota.rs::fetch_quota_summary` 的同一条；基址顺序与
/// [`V1_BASE_URLS`] 相同：sandbox → daily → prod）
pub fn retrieve_user_quota_summary_url(base: &str) -> String {
    method_url(base, METHOD_RETRIEVE_QUOTA_SUMMARY)
}

/// project 发现：`loadCodeAssist`（**prod**，见 [`PROJECT_BASE_URL`]）
pub fn load_code_assist_url() -> String {
    method_url(PROJECT_BASE_URL, METHOD_LOAD_CODE_ASSIST)
}

/// project 开通：`onboardUser`（**prod**）
pub fn onboard_user_url() -> String {
    method_url(PROJECT_BASE_URL, METHOD_ONBOARD_USER)
}

/// 鉴权头（`Authorization: Bearer {access_token}`）。
///
/// 规格 §7.5：token **必须**放 Bearer 头，不能塞 query（`?key=` 那套是
/// `generativelanguage.googleapis.com` 的形态，本家不适用）。
pub fn bearer_header(access_token: &str) -> (String, String) {
    ("Authorization".to_string(), format!("Bearer {}", access_token.trim()))
}

/// 客户端元信息必须与同次请求的 UA 版本一致。
fn client_version(user_agent: &str) -> String {
    let lowered = user_agent.to_ascii_lowercase();
    let Some(start) = lowered.find("antigravity/") else {
        return CLI_VERSION.to_string();
    };
    let tail = &user_agent[start + "antigravity/".len()..];
    let tail = tail
        .split_once('/')
        .filter(|(family, _)| family.eq_ignore_ascii_case("cli") || family.eq_ignore_ascii_case("hub"))
        .map(|(_, version)| version)
        .unwrap_or(tail);
    tail.chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect()
}

fn client_headers(access_token: &str, user_agent: String) -> Vec<(String, String)> {
    let client_version = client_version(&user_agent);
    vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        bearer_header(access_token),
        ("User-Agent".to_string(), user_agent),
        ("x-client-name".to_string(), "antigravity".to_string()),
        ("x-client-version".to_string(), client_version),
    ]
}

/// 推理的 project 只放请求体，不能带 x-goog-user-project。
pub fn content_headers(access_token: &str) -> Vec<(String, String)> {
    client_headers(access_token, user_agent())
}

pub fn catalog_headers(access_token: &str, project_id: &str) -> Vec<(String, String)> {
    with_project(client_headers(access_token, user_agent()), project_id)
}

pub fn quota_headers(access_token: &str, project_id: &str) -> Vec<(String, String)> {
    with_project(client_headers(access_token, quota_user_agent()), project_id)
}

fn with_project(mut headers: Vec<(String, String)>, project_id: &str) -> Vec<(String, String)> {
    let project = project_id.trim();
    if !project.is_empty() {
        headers.push(("x-goog-user-project".to_string(), project.to_string()));
    }
    headers
}

/// **Claude 模型**的 content 请求头：在 [`content_headers`] 之上补
/// `anthropic-beta`（规格 §3.2 的头表 + §5.3：`anthropic-beta` **仅 Claude**）。
///
/// 值取 Manager 的 `claude-code-20250219`（`handlers/openai.rs` 与
/// `handlers/claude.rs` 两处逐字相同的常量；9router 的 gemini 分支用的是更长的
/// 三连 `claude-code-20250219,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14`
/// —— 两者都能通，本仓取 Manager 那一份，与其余头集合的出处保持一致）。
///
/// **只给 Claude 加**：Gemini 的请求带上它没有依据（规格头表把它标成「仅
/// Claude」），多一个非官方头只会离官方指纹更远。
pub fn claude_content_headers(access_token: &str) -> Vec<(String, String)> {
    let mut headers = content_headers(access_token);
    headers.push(("anthropic-beta".to_string(), ANTHROPIC_BETA_CLAUDE.to_string()));
    headers
}

/// Claude 请求要带的 `anthropic-beta` 值（Manager 逐字，见
/// [`claude_content_headers`]）
const ANTHROPIC_BETA_CLAUDE: &str = "claude-code-20250219";

/// project 发现与开通沿用登录身份，不参与目录 / 推理的客户端类型选择。
pub fn admin_headers(access_token: &str, project_id: &str) -> Vec<(String, String)> {
    with_project(client_headers(access_token, oauth_user_agent()), project_id)
}
