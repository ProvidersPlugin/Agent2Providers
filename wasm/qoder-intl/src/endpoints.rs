//! 仅 Site::QoderIntl 的四个固定来源；mode/global 只作兼容，不能切换 provider。
//! 目录固定 api3；唯有对话按 jt- 作业令牌切至 api2。没有国内、千问或自定义地址。
pub const PROVIDER: &str = "qoder-intl";
pub const LABEL: &str = "Qoder 国际版";
pub const WEB: &str = "https://qoder.com";
pub const OPEN_API: &str = "https://openapi.qoder.sh";
pub const GATEWAY: &str = "https://api3.qoder.sh";
pub const JOB_GATEWAY: &str = "https://api2.qoder.sh";
pub const ORIGINS: [&str; 4] = [WEB, OPEN_API, GATEWAY, JOB_GATEWAY];
pub const DEVICE_LOGIN: &str = "/device/selectAccounts";
pub const DEVICE_POLL: &str = "/api/v1/deviceToken/poll";
pub const EXCHANGE: &str = "/api/v1/jobToken/exchange";
pub const USER_INFO: &str = "/api/v1/userinfo";
pub const DEVICE_REFRESH: &str = "/api/v1/deviceToken/refresh";
pub const JOB_REFRESH: &str = "/api/v1/jobToken/refresh";
pub const USAGE: &str = "/api/v2/quota/usage";
pub const PLAN: &str = "/api/v2/user/plan";
pub const CAMPAIGNS: &str = "/sash/api/v1/me/campaigns";
pub const MODELS: &str = "/algo/api/v2/model/list?Encode=1";
pub const CHAT: &str =
    "/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1";
pub const QUEUE_STATUS: &str = "/algo/api/v2/service/ask/queue/status";
pub const QUEUE_FINISH: &str = "/algo/api/v2/service/ask/finish?Encode=1";
pub const COSY_VERSION: &str = "1.1.96-hotfix.8";
pub const CLI_CLIENT_TYPE: &str = "5";
pub const APP_CLIENT_TYPE: &str = "10";
pub const MAX_TOKEN: usize = 64 * 1024;
pub const REFRESH_MARGIN: i64 = 5 * 60_000;
// 缺绝对到期时间时沿用原生兜底，不能据此保证令牌真实有效期。
pub const DEVICE_LIFETIME: i64 = 30 * 86_400_000;
pub const PAT_LIFETIME: i64 = 86_400_000;

pub fn inference(access: &str) -> &'static str {
    if access.starts_with("jt-") {
        JOB_GATEWAY
    } else {
        GATEWAY
    }
}
