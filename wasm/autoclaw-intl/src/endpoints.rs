//! 国际版唯一身份与网络出口，凭据、目录和临时状态不跨地区。
//! 对话使用公开国际插件已确认的 v1 路径（plugins#367），不重试国内备用域。
//! 业务码 400002 的备用续期只改同域路径；目录仍在 /proxy/ 一级。
pub const PROVIDER: &str = "autoclaw-intl";
pub const ORIGIN: &str = "https://autoglm-api.autoglm.ai";
pub const CHAT: &str = "/autoclaw-proxy/proxy/autoclaw/v1/chat/completions";
pub const MODELS: &str = "/autoclaw-proxy/proxy/autoclaw-model-config";
pub const DESKTOP_HELPER: &str = "desktop.credentials.autoclaw-intl.v1";
pub const DESKTOP_ID: &str = "autoclaw-intl-desktop";
pub const ID_PREFIX: &str = "intl-user-";
pub const CLIENT_VERSION: &str = "1.18.5";
pub const CHAT_VERSION: &str = "2.0.3";
pub const MAX_TOKEN: usize = 32 * 1024;
pub const REFRESH_MARGIN: i64 = 300_000;
pub const HOURLY_REFRESH: i64 = 3_600_000;
pub const CALLBACK_PORTS: [u16; 4] = [18432, 19654, 19723, 53699];
