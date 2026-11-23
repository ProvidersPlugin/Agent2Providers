//! QuWork 客户端 1.6.0 使用独立的账号服务与 OpenAI 兼容模型网关。
//! 登录会话的 accessToken 不可代替模型调用的 llmToken。

pub const PROVIDER: &str = "quwork";
pub const CLIENT_VERSION: &str = "1.6.0";
pub const ACCOUNT_ORIGIN: &str = "https://llm.rrzu.com";
pub const MODEL_ORIGIN: &str = "https://qurwork-llm.rrzu.com/v1";
pub const CATALOG_URL: &str =
    "https://img1.rrzuji.cn/qurvis-package/latest/qurvis-desktop-model-config.json";
pub const SELF_PATH: &str = "/api/user/self";
pub const REFRESH_PATH: &str = "/api/user/refresh";
pub const DAILY_STATUS_PATH: &str = "/api/user/daily-reward/status";
pub const DAILY_CLAIM_PATH: &str = "/api/user/daily-reward/claim";
pub const MONTHLY_GIFTS_PATH: &str = "/api/user/benefit/monthly-gifts";
pub const REDEEM_PATH: &str = "/api/user/benefit/redeem";
pub const ACTIVATE_PATH: &str = "/api/user/activate";
pub const MAX_RESPONSE_BYTES: usize = 1_048_576;
pub const REFRESH_WINDOW_MS: i64 = 60 * 60_000;

pub fn headers() -> Vec<(String, String)> {
    vec![
        ("X-QurWork-Env".into(), "prod".into()),
        ("X-QurWork-Version".into(), CLIENT_VERSION.into()),
    ]
}
