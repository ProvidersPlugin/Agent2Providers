//! 来源为现有 QuWork 1.6.0 适配及公开模型配置；尚未做真实账号验收。
//! 模型 llmToken 与账号 accessToken 不可互换，端点不接受环境变量覆盖。

pub const ACCOUNT: &str = "https://llm.rrzu.com";
pub const MODEL: &str = "https://llm.quwork.cn/v1";
pub const LEGACY_MODEL: &str = "https://qurwork-llm.rrzu.com/v1";

pub fn model_origin(value: &str) -> Option<&'static str> {
    match value.trim_end_matches('/') {
        MODEL => Some(MODEL),
        LEGACY_MODEL => Some(LEGACY_MODEL),
        _ => None,
    }
}
pub const CATALOG: &str =
    "https://img1.rrzuji.cn/qurvis-package/latest/qurvis-desktop-model-config.json";
pub const SELF: &str = "/api/user/self";
pub const REFRESH: &str = "/api/user/refresh";
pub const DAILY_STATUS: &str = "/api/user/daily-reward/status";
pub const DAILY_CLAIM: &str = "/api/user/daily-reward/claim";
pub const MONTHLY: &str = "/api/user/benefit/monthly-gifts";
pub const REDEEM: &str = "/api/user/benefit/redeem";
pub const ACTIVATE: &str = "/api/user/activate";
pub const HELPER: &str = "desktop.credentials.quwork.v1";
pub const VERSION: &str = "1.6.0";
pub const MAX_RESPONSE: usize = 1_048_576;
pub const MAX_CREDENTIALS: usize = 262_144;
pub const REFRESH_WINDOW: i64 = 3_600_000;
