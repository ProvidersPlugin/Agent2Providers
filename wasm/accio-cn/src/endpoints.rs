//! 国内身份的唯一常量表。依据原生 accio/endpoints.rs 的 Region::Cn。
//! 网关与 PKCE 客户端相同，登录域、账号身份与 x-package-region 必须改为国内。
//! 不读取环境变量，不声明国际登录域，也不开放动态账号来源。

pub const ID: &str = "accio-cn";
pub const EDITION: &str = "cn";
pub const REGION: &str = "CN";
pub const LABEL: &str = "Accio 国内版";
pub const GATEWAY: &str = "https://phoenix-gw.alibaba.com";
pub const LOGIN: &str = "https://www.accio-ai.com";
pub const CALLBACK: &str = "/auth/callback-accio";
pub const TOKEN: &str = "/api/oauth/token";
pub const REFRESH: &str = "/api/auth/refresh_token";
pub const PROFILE: &str = "/api/auth/userinfo";
pub const QUOTA: &str = "/api/entitlement/quota";
pub const SUBSCRIPTION: &str = "/api/entitlement/currentSubscription";
pub const MODELS: [&str; 2] = ["/api/llm/config", "/api/llm/config/v2"];
pub const CHAT: &str = "/api/adk/llm/generateContent";
pub const CLIENT_ID: &str = "accio-work";
pub const TENANT: &str = "accio-agent";
pub const IAI_TAG: &str = "phoenix-desktop";
pub const VERSION: &str = "0.34.4";
pub const APP_KEY: &str = "OneChat";
pub const USER_AGENT: &str = "agent2api-accio/3.3.0";
