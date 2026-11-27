//! 只保留原生 Site::QoderCn 的事实；不包含国际、千问、VPC 或用户自定义域名。
//! 当前设备授权有效：nonce 必须带 UUID 横线；不走授权码、本机回调或深链。
pub const PROVIDER: &str = "qoder";
pub const LABEL: &str = "Qoder 中国版";
pub const WEB: &str = "https://qoder.com.cn";
pub const OPEN_API: &str = "https://openapi.qoder.com.cn";
pub const GATEWAY: &str = "https://gateway.qoder.com.cn";
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
pub const CHAT: &str = "/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1";
pub const COSY_VERSION: &str = "1.1.79";
pub const CLI_CLIENT_TYPE: &str = "5";
pub const APP_CLIENT_TYPE: &str = "10";
pub const MAX_TOKEN: usize = 64 * 1024;
pub const REFRESH_MARGIN: i64 = 5 * 60_000;
pub const DEVICE_LIFETIME: i64 = 30 * 86_400_000;
pub const PAT_LIFETIME: i64 = 86_400_000;
