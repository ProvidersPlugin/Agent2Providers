//! Command Code 上游地址、协议版本与**逐字请求头**。
//!
//! ── 上游长什么样（从网关 `providers/commandcode/endpoints.rs` 原样搬来）──
//! ```text
//!   POST /alpha/generate              聊天生成（唯一生成入口）
//!   POST /alpha/fingerprint/record    设备指纹上报（每 key 首次 + 每 8h+2h）
//!   POST /alpha/lifecycle-events      生命周期事件上报（与指纹并行、同频）
//!   GET  /provider/v1/models          模型目录（5 分钟缓存，失败回落内置清单）
//!   GET  /alpha/billing/credits       额度 / 限流窗口（本家只用它做 key 探活）
//! ```
//! 单一域名 `https://api.commandcode.ai`，**没有国内 / 国际两套域名**，
//! 因此本家是单 provider、单地区。
//!
//! ── 以官方 CLI 的形态请求：头集合就是协议的一部分 ─────────────────────
//! 上游按「像不像官方 CLI」做风控，参考实现逐字对齐了 CLI 的常量表：
//!   - `User-Agent: cli`（**不是** reqwest 默认值）；
//!   - `x-command-code-version` 报「**已实现的**协议版本」而不是 npm 上的最新版；
//!   - 指纹预请求与生成请求**共用同一张头表**；
//!   - **不发 `x-co-flag`**（不在 CLI 常量表里，发出去是破绽）。

/// 默认基址
pub const DEFAULT_BASE_URL: &str = "https://api.commandcode.ai";

/// 本插件**实际实现**的 wire 协议版本（对齐 `command-code@1.53.1` 的方言）。
///
/// 不要写成「npm 上的最新版」：这个值是自称的协议版本，代码里的字段形状才是
/// 实际方言；两者一旦分叉，上报的版本号就成了自相矛盾的证据。
pub const PROTOCOL_VERSION: &str = "1.53.1";

/// 官方 CLI 的 User-Agent 常量（一个字都不能改）
pub const CLI_USER_AGENT: &str = "cli";

/// CLI 环境标识的默认值
pub const CLI_ENVIRONMENT: &str = "production";

/// 生成请求的路径
pub const GENERATE_PATH: &str = "/alpha/generate";
/// 设备指纹上报路径
pub const FINGERPRINT_PATH: &str = "/alpha/fingerprint/record";
/// 生命周期事件路径
pub const LIFECYCLE_PATH: &str = "/alpha/lifecycle-events";
/// 模型目录路径
pub const MODELS_PATH: &str = "/provider/v1/models";
/// 额度 / 限流窗口路径（key 探活用）
pub const BILLING_CREDITS_PATH: &str = "/alpha/billing/credits";

/// 读环境变量覆盖（空值视为未设置，去掉尾部斜杠）
fn env_override(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
}

/// 基址：`COMMANDCODE_API_BASE` 或 `CC_API_BASE` 可覆盖，前者优先。
pub fn base_url() -> String {
    env_override("COMMANDCODE_API_BASE")
        .or_else(|| env_override("CC_API_BASE"))
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
}

/// 协议版本：`COMMANDCODE_PROTOCOL_VERSION` / `CC_VERSION` 可覆盖
pub fn protocol_version() -> String {
    env_override("COMMANDCODE_PROTOCOL_VERSION")
        .or_else(|| env_override("CC_VERSION"))
        .unwrap_or_else(|| PROTOCOL_VERSION.to_string())
}

/// CLI 环境标识：`COMMANDCODE_CLI_ENVIRONMENT` / `CC_CLI_ENVIRONMENT` 可覆盖
pub fn cli_environment() -> String {
    env_override("COMMANDCODE_CLI_ENVIRONMENT")
        .or_else(|| env_override("CC_CLI_ENVIRONMENT"))
        .unwrap_or_else(|| CLI_ENVIRONMENT.to_string())
}

/// 全 URL（含路径）
pub fn url(path: &str) -> String {
    format!("{}{path}", base_url())
}

/// 一家共用的 CLI 客户端头（生成 / 指纹 / 生命周期三处都要这几项）。
///
/// `User-Agent` 显式给 `cli`：reqwest 的默认 UA 会自报「我不是 CLI」。
pub fn cli_headers() -> Vec<(String, String)> {
    vec![
        ("User-Agent".to_string(), CLI_USER_AGENT.to_string()),
        ("x-command-code-version".to_string(), protocol_version()),
        ("x-cli-environment".to_string(), cli_environment()),
    ]
}

/// 鉴权头（`Authorization: Bearer <user_key>`）
pub fn auth_headers(api_key: &str) -> Vec<(String, String)> {
    vec![(
        "Authorization".to_string(),
        format!("Bearer {}", api_key.trim()),
    )]
}

/// 条件头 `x-cmd-zdr: 1`（零数据留存；仅当客户端带了 `x-cmd-zdr: 1` 时下发）。
pub fn zdr_header(
    client_headers: &std::collections::BTreeMap<String, String>,
) -> Option<(String, String)> {
    let requested = client_headers
        .get("x-cmd-zdr")
        .map(|value| value.trim())
        .unwrap_or("");
    (requested == "1").then(|| ("x-cmd-zdr".to_string(), "1".to_string()))
}
