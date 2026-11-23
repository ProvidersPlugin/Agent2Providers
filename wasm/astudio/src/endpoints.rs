//! 地址来自原生AStudio/endpoints.rs、auth.rs与桌面端协议记录。
//! 本家无已确认的refresh接口；不试探/拼接续期或额外领取路径。

pub const MODEL_ORIGIN: &str = "https://astronstudio-api-volces-prod.xf-yun.com";
pub const RESPONSES: &str = "/model-gateway/v1/responses";
pub const MODELS: &str = "/api/v1/model-manager/models";
pub const STUDIO: &str = "https://agent.xfyun.cn/xingchen-studio/";
pub const VERSION: &str = "3.5.5";
pub const CAPTCHA: &str = "chat/gee-captcha";
pub const SEND_SMS: &str = "login/mobile/send-verify-code";
pub const LOGIN_SMS: &str = "login/phone-quick-login";
pub const LOGIN_PASSWORD: &str = "login/check-account";
pub const INIT_APP: &str = "tenant-app/v2/init-app";
pub const USER_INFO: &str = "userInfo";
pub const MODEL_CONFIGS: &str = "bot/models/configs";
pub const BALANCE: &str = "points/balance";
pub const MEMBERSHIP: &str = "membership/me";
pub const PLAN: &str = "membership/plan-config";
pub const DESKTOP_HELPER: &str = "desktop.credentials.astudio.v1";
