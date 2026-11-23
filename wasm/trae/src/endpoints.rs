//! 所有地址逐项来自原生 trae/{mod,oauth,refresh,profile,usage,checkin}.rs。
//! CN/IDE 与 SOLO 共用本 provider；国际版不能复用国内身份、凭据或目录。
//! 未发现已确认的新手领取端点，只有每日 checkin；不由路径命名猜测活动接口。

pub const AGENT: &str = "https://trae-api-cn.mchost.guru";
pub const CHAT: &str = "/api/agent/v3/llm_utils_chat";
pub const MODELS: &str = "/api/ide/v1/get_detail_param";
pub const BATCH_MODELS: &str = "/api/ide/v1/batch_get_detail_param";
pub const ACCOUNT: &str = "https://api.trae.cn";
pub const ACCOUNT_ORIGINS: [&str; 3] = [ACCOUNT, "https://api.trae.com.cn", "https://www.trae.cn"];
pub const GUIDANCE: &str = "/cloudide/api/v3/trae/GetLoginGuidance";
pub const EXCHANGE: &str = "/trae/api/v3/oauth/ExchangeToken";
pub const REFRESH: &str = "/cloudide/api/v3/trae/oauth/ExchangeToken";
pub const PROFILE: &str = "/cloudide/api/v3/trae/GetUserInfo";
pub const ENT_USAGE: &str = "/trae/api/v2/pay/ide_user_ent_usage";
pub const PAY_STATUS: &str = "/trae/api/v1/pay/ide_user_pay_status";
pub const CHECKIN_STATUS: &str = "/trae/api/v2/ug/checkin_credits/status";
pub const CHECKIN_CLAIM: &str = "/trae/api/v2/ug/checkin_credits/claim";
pub const CALLBACK_PATH: &str = "/authorize";
pub const IDE_VERSION: &str = "0.1.69";
pub const VERSION_CODE: &str = "20260917";
pub const APP_ID: &str = "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8";
pub const SOLO_CLIENT: &str = "en1oxy7wnw8j9n";
pub const IDE_CLIENT: &str = "ono9krqynydwx5";

pub fn client_id(variant: &str) -> &'static str {
    if variant == "cn" {
        IDE_CLIENT
    } else {
        SOLO_CLIENT
    }
}

pub fn platform_code(variant: &str) -> &'static str {
    if variant == "cn" {
        "IDE_PC"
    } else {
        "SOLO_PC"
    }
}
