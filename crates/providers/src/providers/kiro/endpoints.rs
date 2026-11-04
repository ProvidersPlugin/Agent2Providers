//! Kiro（kiro.dev）的地址与协议常量。
//!
//! ── 依据 ────────────────────────────────────────────────────
//! 参考实现为 magpie 社区的 `@magpie-community/opencode-kiro-auth`
//! （`Acankao/magpie-plugins/packages/kiro`，MIT）。本目录把它改写成
//! OpenAI Chat 入站、OpenAI Chat SSE 出站的 Rust 版本。
//!
//! ```text
//!   登录（Kiro 门户）   https://app.kiro.dev/signin
//!   换码（Google/GitHub）https://prod.us-east-1.auth.desktop.kiro.dev/oauth/token
//!   刷新（社交登录）     https://prod.{region}.auth.desktop.kiro.dev/refreshToken
//!   AWS OIDC            https://oidc.{region}.amazonaws.com
//!   控制面（目录/用量）  https://management.{region}.kiro.dev
//!   推理                https://runtime.{region}.kiro.dev/generateAssistantResponse
//! ```
//!
//! ── 地区 ────────────────────────────────────────────────────
//! Kiro 只有两个订阅地区：`us-east-1` 与 `eu-central-1`。账号的 profile ARN
//! (`arn:aws:codewhisperer:us-east-1:…`) 里带着地区，登录时用的地区作为
//! 兜底。与「两地是两个 provider」的家不同：两边共用同一套账号体系与令牌，
//! 只是端点不同，所以**不拆家**。

/// Kiro 门户（浏览器登录入口）。
pub const PORTAL: &str = "https://app.kiro.dev";

/// 社交登录换码服务（与地区无关：门户固定 us-east-1 的 auth 服务）。
pub const AUTH_SERVICE: &str = "https://prod.us-east-1.auth.desktop.kiro.dev";

/// Builder ID 的服务 profile（Builder ID 没有自己的 profile，官方 IDE 与
/// kiro-cli 都用这一个）。
pub const BUILDER_ID_START: &str = "https://view.awsapps.com/start";

/// Kiro IDE / kiro-cli 给 Builder ID 用的 profile。
pub const BUILDER_ID_PROFILE: &str =
    "arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX";

/// 登录页跳回的本机端口（Kiro IDE 监听的那一组）。
pub const CALLBACK_PORTS: &[u16] = &[
    3128, 4649, 6588, 8008, 9091, 49153, 50153, 51153, 52153, 53153,
];

/// 向 AWS 申请的 scopes（与 Kiro IDE 一致）。
pub const SCOPES: &[&str] = &[
    "codewhisperer:completions",
    "codewhisperer:analysis",
    "codewhisperer:conversations",
    "codewhisperer:transformations",
    "codewhisperer:taskassist",
];

/// 刷新窗口：请求前 token 距过期不足这么久就先刷。
pub const EARLY_MS: i64 = 2 * 60 * 1000;

/// 后台续期的提前量（网关的 `token.expiring` 用它）。
pub const LEAD_MS: i64 = 10 * 60 * 1000;

/// 登录流程的最长等待时间。
pub const SIGN_IN_TIMEOUT_MS: u64 = 10 * 60 * 1000;

/// 镜像：`KiroIDE-<version>-<hash>`。版本随官方 IDE 走，hash 由主机名派生
/// （与参考实现同一形态：它用 sha256("magpie:" + hostname)）。
pub fn ide_user_agent(hostname: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"agent2api:");
    hasher.update(hostname.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("KiroIDE-1.1.70-{hex}")
}

/// 刷新社交登录时 Kiro 桌面端自报的 UA。
pub const DESKTOP_UA: &str = "Kiro-Desktop/0.2.13 (darwin; arm64)";

/// 控制面地址。
pub fn management(region: &str) -> String {
    format!("https://management.{region}.kiro.dev")
}

/// 推理地址。
pub fn runtime(region: &str) -> String {
    format!("https://runtime.{region}.kiro.dev")
}

/// AWS OIDC 地址。
pub fn oidc(region: &str) -> String {
    format!("https://oidc.{region}.amazonaws.com")
}

/// 社交登录的刷新地址。
pub fn refresh_url(region: &str) -> String {
    format!("https://prod.{region}.auth.desktop.kiro.dev/refreshToken")
}

/// 从 profile ARN 里取地区；取不到时按登录地区判（EU → eu-central-1）。
pub fn region_of(profile: &str, signed_in: &str) -> String {
    let parts: Vec<&str> = profile.split(':').collect();
    if parts.len() > 4 && !parts[3].is_empty() {
        return parts[3].to_string();
    }
    if signed_in.starts_with("eu-") {
        "eu-central-1".to_string()
    } else {
        "us-east-1".to_string()
    }
}

/// 一个合法的 AWS 地区名（只按形态判：它要拼进 AWS 的主机名）。
pub fn is_aws_region(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 9 || bytes.len() > 32 {
        return false;
    }
    let mut parts = value.split('-');
    let (Some(first), Some(second)) = (parts.next(), parts.next()) else {
        return false;
    };
    if first.len() != 2 || !first.chars().all(|ch| ch.is_ascii_lowercase()) {
        return false;
    }
    if second.is_empty() || !second.chars().all(|ch| ch.is_ascii_lowercase()) {
        return false;
    }
    let rest: Vec<&str> = parts.collect();
    if rest.len() != 2 {
        return false;
    }
    !rest[0].is_empty()
        && rest[0].chars().all(|ch| ch.is_ascii_lowercase())
        && !rest[1].is_empty()
        && rest[1].chars().all(|ch| ch.is_ascii_digit())
}

/// 是 https 且不带用户名的地址（AWS Start URL 的形态检查）。
pub fn is_https_url(value: &str) -> bool {
    let trimmed = value.trim();
    let Some(rest) = trimmed.strip_prefix("https://") else {
        return false;
    };
    if rest.is_empty() || rest.starts_with('/') {
        return false;
    }
    let authority = rest.split('/').next().unwrap_or("");
    !authority.is_empty() && !authority.contains('@')
}

/// AWS JSON 1.0 的 `x-amz-target`（控制面操作名）。
pub const TARGET_USAGE_LIMITS: &str = "AmazonCodeWhispererService.GetUsageLimits";
pub const TARGET_KEY_MODELS: &str = "KiroControlPlaneBearerService.ListAvailableModels";
