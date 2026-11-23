//! 生产来源来自当前原生1.1.5实现，不启用env、自定义域或隐式凭据旁路。
pub const ORIGIN: &str = "https://xiaohuanxiong.com";
pub const AUTH: &str = "/api/web/auth/v1";
pub const LLM: &str = "/api/web/llm/v2";
pub const HELPER: &str = "desktop.credentials.raccoon.v1";
pub const REFRESH_HELPER: &str = "desktop.refresh.raccoon.v1";
pub const DESKTOP_ID: &str = "raccoon-desktop";
pub const MAX_CREDENTIALS: usize = 256 * 1024;
