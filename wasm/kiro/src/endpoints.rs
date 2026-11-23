//! 固定地址均来自原生 kiro/endpoints.rs，未回退到历史 q/codewhisperer 猜测域。
//! Kiro 两个订阅区域共用账号，仍只有 kiro 身份。OIDC 认证目录使用官方精确清单；
//! 外部 IdP 续期使用宿主受控入口，而非普通网络白名单。

use agent2api_wasm_sdk::{Error, Result};

pub const ID: &str = "kiro";
pub const PORTAL: &str = "https://app.kiro.dev";
pub const BUILDER_START: &str = "https://view.awsapps.com/start";
pub const BUILDER_PROFILE: &str =
    "arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX";
pub const REGIONS: [&str; 2] = ["us-east-1", "eu-central-1"];
pub const PORTS: [u16; 10] = [
    3128, 4649, 6588, 8008, 9091, 49153, 50153, 51153, 52153, 53153,
];
pub const SCOPES: [&str; 5] = [
    "codewhisperer:completions",
    "codewhisperer:analysis",
    "codewhisperer:conversations",
    "codewhisperer:transformations",
    "codewhisperer:taskassist",
];
pub const EARLY: i64 = 120_000;
pub const LEAD: i64 = 600_000;
pub const LOGIN_TTL: i64 = 600_000;
pub const USAGE_TARGET: &str = "AmazonCodeWhispererService.GetUsageLimits";
pub const MODELS_TARGET: &str = "KiroControlPlaneBearerService.ListAvailableModels";
pub const SEARCH_TARGET: &str = "AmazonCodeWhispererStreamingService.InvokeMCP";

pub fn checked(region: &str) -> Result<&str> {
    if REGIONS.contains(&region) {
        Ok(region)
    } else {
        Err(
            Error::http(400, "此 AWS 地区尚未获得精确域名授权；不能改用其他地区代替")
                .with_code("kiro_region_not_authorized"),
        )
    }
}
pub fn region_of(profile: &str, signed_in: &str) -> Result<String> {
    if !profile.is_empty() {
        let parts: Vec<_> = profile.split(':').collect();
        if parts.len() < 6
            || parts[0] != "arn"
            || parts[1] != "aws"
            || parts[2] != "codewhisperer"
            || parts[3].is_empty()
        {
            return Err(Error::http(400, "Kiro profile ARN 格式无效"));
        }
        return checked(parts[3]).map(str::to_string);
    }
    // 与原生相同：登录目录地区只决定 EU / US 业务落点，不把认证区域混成业务区域。
    Ok(if signed_in.starts_with("eu-") {
        "eu-central-1"
    } else {
        "us-east-1"
    }
    .into())
}
pub fn management(region: &str) -> Result<String> {
    Ok(format!("https://management.{}.kiro.dev", checked(region)?))
}
pub fn runtime(region: &str) -> Result<String> {
    Ok(format!("https://runtime.{}.kiro.dev", checked(region)?))
}
pub fn auth_region(region: &str) -> Result<&str> {
    if agent2api_wasm_sdk::aws_oidc::origin(region).is_some() {
        Ok(region)
    } else {
        Err(Error::http(400, "AWS 认证目录地区未列在官方精确来源清单中")
            .with_code("kiro_region_not_authorized"))
    }
}
pub fn oidc(region: &str) -> Result<String> {
    auth_region(region)?;
    agent2api_wasm_sdk::aws_oidc::origin(region).map(str::to_string)
        .ok_or_else(|| Error::http(400, "AWS 认证目录地区无效"))
}
pub fn refresh(region: &str) -> Result<String> {
    Ok(format!("https://prod.{}.auth.desktop.kiro.dev/refreshToken", checked(region)?))
}
pub fn https(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 8192
        && !raw.bytes().any(|ch| ch <= b' ' || ch == 127 || ch == b'\\')
        && url::Url::parse(raw).is_ok_and(|url| {
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
        })
}
