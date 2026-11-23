//! 证据为旧原生 accio/endpoints.rs：两地业务网关相同，登录域和地域头不同。
//! 国际只有 accio / intl / GLOBAL；没有动态账号域，禁止环境变量改写网关。

pub const ID: &str = "accio";
pub const EDITION: &str = "intl";
pub const REGION: &str = "GLOBAL";
pub const GATEWAY: &str = "https://phoenix-gw.alibaba.com";
pub const LOGIN: &str = "https://www.accio.com";
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
