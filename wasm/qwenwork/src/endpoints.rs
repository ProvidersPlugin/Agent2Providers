//! 只保留原生 Site::QwenworkCn；网页、账号、目录、推理均固定同一 gateway。
//! redirect_uri 是设备授权的已登记参数，不在本机监听回调。没有 PAT、作业令牌或活动端点。
pub const PROVIDER: &str = "qwenwork";
pub const LABEL: &str = "千问办公 国内版";
pub const ORIGIN: &str = "https://gateway.qwenwork.cn";
pub const CLIENT_ID: &str = "e883ade2-e6e3-4d6d-adf7-f92ceff5fdcb";
pub const REDIRECT_URI: &str = "qwenwork-cn://";
pub const DEVICE_LOGIN: &str = "/device/selectAccounts";
pub const DEVICE_POLL: &str = "/api/v1/deviceToken/poll";
pub const DEVICE_REFRESH: &str = "/api/v1/deviceToken/refresh";
pub const USER_INFO: &str = "/api/v1/adapter/user/account-context?include=user,plan,quota,page,data_sharing";
pub const USAGE: &str = "/api/v1/adapter/user/account-context?include=user,plan,quota";
pub const WALLETS: &str = "/api/v1/adapter/user/wallets";
pub const MODELS: &str = "/algo/api/v2/model/list?Encode=1";
pub const CHAT: &str =
    "/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1";
pub const COSY_VERSION: &str = "1.1.52";
pub const CLIENT_TYPE: &str = "6";
pub const DATA_POLICY: &str = "agree";
pub const SCENE: &str = "qwork";
pub const PRODUCT: &str = "qoder_work";
pub const BUSINESS_TYPE: &str = "agent";
pub const STAGE: &str = "processing";
pub const USER_AGENT: &str = "qoderwork/1.1.0";
pub const REFRESH_TARGET: &str = "c";
pub const MAX_TOKEN: usize = 64 * 1024;
pub const REFRESH_MARGIN: i64 = 60 * 60_000;
// 只在上游没给绝对到期时间时保留原生兜底；不是实际令牌有效期的保证。
pub const DEVICE_LIFETIME: i64 = 30 * 86_400_000;
pub const STATIC_CONTEXT: i64 = 131_072;
// 仅远程条目缺 max_input_tokens 时的客户端原生默认，不覆盖静态三项或远程真实值。
pub const REMOTE_CONTEXT_FALLBACK: i64 = 1_024_000;
pub const MAX_OUTPUT: i64 = 32_768;
