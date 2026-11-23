//! 端点/客户端版本来自原生 endpoints.rs；三环境不是地区，不添加任意 Google 域名。
//! 原生环境变量覆盖不在 WASM 中读取。账号显式 UA/代理由主程序裁决。

use super::util;
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub const CLIENT_ID: &str = concat!(
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep",
    ".apps.googleusercontent.com"
);
pub const CLIENT_SECRET: &str = concat!("GOCSPX-K58FWR486LdLJ1mLB8sXC", "4z6qDAf");
pub const AUTH: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN: &str = "https://oauth2.googleapis.com/token";
pub const USERINFO: [&str; 2] = [
    "https://www.googleapis.com/oauth2/v2/userinfo",
    "https://www.googleapis.com/oauth2/v1/userinfo",
];
pub const SCOPES: [&str; 6] = [
    "openid",
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];
pub const SANDBOX: &str = "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal";
pub const DAILY: &str = "https://daily-cloudcode-pa.googleapis.com/v1internal";
pub const PROD: &str = "https://cloudcode-pa.googleapis.com/v1internal";
/// 控制面查询（目录与配额）优先 `daily` 宿主，避免部分账号在 `sandbox` 403/缺组。
pub const CONTROL_BASES: [&str; 3] = [DAILY, SANDBOX, PROD];
pub const CALLBACK_PATH: &str = "/oauth-callback";
pub const CLI_VERSION: &str = "1.5.1";
pub const HUB_VERSION: &str = "2.28.0";
pub const OAUTH_VERSION: &str = "4.3.0";

pub fn method(base: &str, name: &str) -> String {
    format!("{base}:{name}")
}
pub fn stream(base: &str) -> String {
    format!("{base}:streamGenerateContent?alt=sse")
}
pub fn oauth_ua() -> String {
    format!("vscode/1.X.X (Antigravity/{OAUTH_VERSION})")
}

fn platform() -> Result<&'static str> {
    match sdk::host("runtime.platform", json!({}))?.as_str() {
        Some("win") => Ok("windows"),
        Some("mac") => Ok("darwin"),
        Some("linux") => Ok("linux"),
        Some("other") => Ok("other"),
        _ => Err(Error::new("主程序未返回有效系统类型")),
    }
}

fn architecture() -> Result<String> {
    let value = sdk::host("runtime.architecture", json!({}))?;
    let arch = value
        .as_str()
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 64
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
        })
        .ok_or_else(|| Error::new("主程序未返回有效处理器架构"))?;
    Ok(match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
    .into())
}

#[derive(Clone, Copy)]
pub enum Plane {
    Content,
    Catalog,
    Quota,
    Admin,
}

pub fn headers(token: &str, project: &str, plane: Plane, claude: bool) -> Result<Value> {
    let token = util::header(token, 16_384)?;
    if token.is_empty() {
        return Err(Error::http(401, "Antigravity 缺少访问令牌"));
    }
    let (ua, version) = match plane {
        Plane::Admin => (oauth_ua(), OAUTH_VERSION),
        Plane::Quota => (
            format!(
                "antigravity/hub/{HUB_VERSION} (aidev_client; os_type={}; arch={})",
                platform()?,
                architecture()?
            ),
            HUB_VERSION,
        ),
        _ => (
            format!(
                "antigravity/cli/{CLI_VERSION} {}/{}",
                platform()?,
                architecture()?
            ),
            CLI_VERSION,
        ),
    };
    let mut headers = json!({ "Content-Type": "application/json", "Accept": "application/json",
        "Authorization": format!("Bearer {token}"), "User-Agent": ua,
        "x-client-name": "antigravity", "x-client-version": version });
    if !matches!(plane, Plane::Content) && !project.trim().is_empty() {
        headers["x-goog-user-project"] = json!(util::header(project, 1024)?);
    }
    if matches!(plane, Plane::Content) && claude {
        headers["anthropic-beta"] = json!("claude-code-20250219");
    }
    // 不发 x-goog-api-client；content 的 project 只在信封中。
    Ok(headers)
}
