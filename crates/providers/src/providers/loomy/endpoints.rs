//! Loomy（讯飞）上游地址与**客户端内置签名对**（环境变量可覆盖）。
//!
//! ── 上游长什么样（依据：Loomy 桌面端 `app.asar`）──────────
//! ```text
//!   账号 CAccount     https://account.xfinfr.com        （HMAC-SHA1 签名头）
//!   集成网关（积分）  https://loomyad.xunfei.cn          （明文 session 放 `token` 头）
//!   模型网关          {集成网关}/api/v1                  （OpenAI 兼容，token + Bearer 双头）
//! ```
//!
//! 前两个值来自安装包内加密的 `.env.prod`；模型网关没有独立配置 —— 客户端把
//! 它拼成「积分基址 + `/api/v1`」。
//!
//! ── 签名对为什么内置 ────────────────────────────────────────
//! CAccount 的每个请求都要用**客户端内置的** AccessKey 对签 HMAC-SHA1
//! （随安装包分发、不是用户账号凭证），没有它登录接口一个都调不通。
//! 与网关内置版同一处置：内置为默认值，环境变量可覆盖，改动不需要重新编译。

/// 账号（CAccount）默认基址
pub const DEFAULT_XFYUN_BASE_URL: &str = "https://account.xfinfr.com";

/// 集成网关默认基址（积分、每日登录刷新与模型网关都挂在这个域下）
pub const DEFAULT_INTEGRATION_BASE_URL: &str = "https://loomyad.xunfei.cn";

/// 客户端内置 AccessKeyId
pub const DEFAULT_ACCESS_KEY_ID: &str = "2thryby66wxi53sk";

/// 客户端内置 AccessKeySecret
pub const DEFAULT_ACCESS_KEY_SECRET: &str = "zsak6eadrbawz683wf5r3m2snrwj868r";

/// 客户端 AppID（请求信封 `base.appid`；Loomy 桌面端的固定值）
pub const DEFAULT_APP_ID: &str = "GM3LOOMY";

/// 读环境变量覆盖（空值视为未设置），去掉尾部斜杠
fn env_override(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
}

/// 账号（CAccount）基址
pub fn xfyun_base_url() -> String {
    env_override("LOOMY_XFYUN_BASE_URL").unwrap_or_else(|| DEFAULT_XFYUN_BASE_URL.to_string())
}

/// 集成网关基址
pub fn integration_base_url() -> String {
    env_override("LOOMY_POINTS_BASE_URL")
        .unwrap_or_else(|| DEFAULT_INTEGRATION_BASE_URL.to_string())
}

/// 模型网关基址：`{集成网关}/api/v1`
pub fn model_base_url() -> String {
    format!("{}/api/v1", integration_base_url())
}

/// 签名用 AccessKeyId
pub fn access_key_id() -> String {
    env_override("LOOMY_XFYUN_ACCESS_KEY_ID").unwrap_or_else(|| DEFAULT_ACCESS_KEY_ID.to_string())
}

/// 签名用 AccessKeySecret
pub fn access_key_secret() -> String {
    env_override("LOOMY_XFYUN_ACCESS_KEY_SECRET")
        .unwrap_or_else(|| DEFAULT_ACCESS_KEY_SECRET.to_string())
}

/// 请求信封 `base.appid`
pub fn app_id() -> String {
    env_override("LOOMY_XFYUN_APP_ID").unwrap_or_else(|| DEFAULT_APP_ID.to_string())
}
