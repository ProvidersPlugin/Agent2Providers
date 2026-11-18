//! Site::QwenworkIntl：唯一正式来源及两项授权参数有原生 EchoBird 来源证据。
//! 其余客户端常量/账号接口/续期/目录沿用原生的同产品推断，不能当成国际实测结论。
//! 无钱包路径（国际未取证）、作业令牌、PAT 或活动端点；redirect 参数不代表本机回调监听。
pub const PROVIDER: &str = "qwenwork-intl";
pub const LABEL: &str = "千问办公 国际版";
pub const ORIGIN: &str = "https://gateway.qwenwork.ai";
pub const CLIENT_ID: &str = "cc65e5fc-05bd-4f5d-a4e8-0df19aa3d75a";
pub const REDIRECT_URI: &str = "qwenwork://";
pub const DEVICE_LOGIN: &str = "/device/selectAccounts";
pub const DEVICE_POLL: &str = "/api/v1/deviceToken/poll";
pub const DEVICE_REFRESH: &str = "/api/v1/deviceToken/refresh";
pub const USER_INFO: &str =
    "/api/v1/adapter/user/account-context?include=user,plan,quota,page,data_sharing";
pub const USAGE: &str = "/api/v1/adapter/user/account-context?include=user,plan,quota";
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
// 国际设备材料的到期时间以服务端绝对值、标准秒数或JWT提示为准，不虚构固定30天。
pub const STATIC_CONTEXT: i64 = 131_072;
// 仅远程条目未声明长度时的基线推断值，不覆盖静态三项或远程实值。
pub const REMOTE_CONTEXT_FALLBACK: i64 = 1_024_000;
pub const MAX_OUTPUT: i64 = 32_768;
pub const EVIDENCE_NOTE: &str =
    "国际主机及授权参数有原生来源证据；续期、COSY、目录和额度字段多处沿用原生基线推断，尚未在国际账号实测";
pub const WALLETS_NOTE: &str = "国际钱包接口未取证，本插件不请求或补造分池余额；不代表上游没有钱包";
