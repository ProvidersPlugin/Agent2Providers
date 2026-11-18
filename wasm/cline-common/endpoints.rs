//! Cline 共用协议常量；池身份由独立 crate 决定，绝不在此选择其他提供商账号。

pub const API: &str = "https://api.cline.bot/api/v1";
pub const WORKOS: &str = "https://api.workos.com";
pub const CLIENT_ID: &str = "client_01K3A541FN8TA3EPPHTD2325AR";
pub const VERIFY: &str = "https://authkit.cline.bot/device";
pub const CLIENT_TYPE: &str = "cline-sdk";
pub const PREFIX: &str = "workos:";
pub const DESKTOP_HELPER: &str = "desktop.credentials.cline.v1";
pub const REFRESH_MARGIN: i64 = 600_000;
pub const MAX_TOKEN: usize = 8192;
pub const MAX_RESPONSE: usize = 4 * 1024 * 1024;
