//! 基址是账号凭据的一部分；只保存官方 HTTPS/443 基址，绝不改写到示例网关。
//! 动态模型请求只走 http.account-origin.v1，宿主从当前账号 secret.baseUrl 授予精确来源。
//! 不在普通 origins 中列任何模型网关，避免静态白名单绕过逐账号授权。

use agent2api_wasm_sdk::{Error, Result};

pub const CLOUD: &str = "https://agentarts.cn-southwest-2.myhuaweicloud.com";
pub const AUTH: &str = "https://auth.huaweicloud.com";
pub const IAM: &str = "https://iam.myhuaweicloud.com";
pub const TOKEN: &str = "https://sts.cn-north-4.myhuaweicloud.com/v1/oauth2/tokens";
pub const CLIENT_ID: &str = "pdp5_for_agentarts";
pub const CALLBACK: &str =
    "https://agentarts.cn-southwest-2.myhuaweicloud.com/v1/claw/auth/callback";
pub const REGION: &str = "cn-southwest-2";
const SUFFIX: &str = ".officeace.cn-southwest-2.huaweicloud-agentarts.com";

/// 身份算法必须保留旧规范化行为，不用 Url 序列化悄悄改变大小写或路径。
pub fn normalize_base(base: &str) -> String {
    let trimmed = base.trim().trim_end_matches('/');
    if trimmed.is_empty() || trimmed.ends_with("/v2") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v2")
    }
}

pub fn official_base(raw: &str) -> Result<String> {
    let candidate = normalize_base(raw);
    let url =
        url::Url::parse(&candidate).map_err(|_| Error::http(400, "OfficeAce 模型网关地址无效"))?;
    if !candidate.starts_with("https://")
        || candidate.contains('\\')
        || candidate
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
        || url.scheme() != "https"
        || url.as_str() != candidate
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/v2"
        || !url
            .host_str()
            .is_some_and(|host| host.ends_with(SUFFIX) && host.len() > SUFFIX.len())
    {
        return Err(Error::http(
            400,
            "此 WASM 插件只接受官方果办 HTTPS 模型网关，不会将凭据发送到自定义地址",
        )
        .with_code("officeace_gateway_invalid"));
    }
    Ok(candidate)
}

pub fn gateway(raw: &str, path: &str) -> Result<String> {
    if !matches!(
        path,
        "/v2/models"
            | "/v2/chat/completions"
            | "/v1/subscription"
            | "/v1/model-token-rates"
            | "/v1/subscription/bonus/claim"
    ) {
        return Err(Error::http(400, "OfficeAce 模型网关请求路径未获允许"));
    }
    let base = official_base(raw)?;
    let root = base.strip_suffix("/v2").unwrap_or(&base);
    Ok(format!("{root}{path}"))
}

pub fn is_gateway(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(|host| host.ends_with(SUFFIX)))
        .unwrap_or(false)
}
