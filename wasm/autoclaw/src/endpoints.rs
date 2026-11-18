//! AutoClaw 国内版的唯一端点与身份来源，不尝试国际版备用域。
//! 目录位于 /proxy/ 一级，不能拼到聊天的 /proxy/autoclaw/ 下。
pub const PROVIDER: &str = "autoclaw";
pub const ORIGIN: &str = "https://autoglm-acceleration-api.zhipuai.cn";
pub const CHAT: &str = "/autoclaw-proxy/proxy/autoclaw/chat/completions";
pub const MODELS: &str = "/autoclaw-proxy/proxy/autoclaw-model-config";
pub const DESKTOP_HELPER: &str = "desktop.credentials.autoclaw.v1";
pub const DESKTOP_ID: &str = "autoclaw-desktop";
pub const ID_PREFIX: &str = "user-";
pub const CLIENT_VERSION: &str = "1.18.5";
pub const CHAT_VERSION: &str = "1.17.8";
pub const MAX_TOKEN: usize = 32 * 1024;
pub const REFRESH_MARGIN: i64 = 300_000;
pub const HOURLY_REFRESH: i64 = 3_600_000;
