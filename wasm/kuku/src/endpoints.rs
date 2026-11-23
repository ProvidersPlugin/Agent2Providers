//! 地址来自当前原生kuku实际执行路径，不采用未编译mod_provider.rs或旧注释。
//! 原生认证BDUSS无法刷新，passport仅换产品STOKEN，不能冒称恢复百度登录态。

pub const ID: &str = "kuku";
pub const BASE: &str = "https://kuku.baidu.com";
pub const PASSPORT: &str = "https://passport.baidu.com/v3/login/api/auth";
pub const APP_ID: i64 = 123971023;
pub const CHANNEL: &str = "chunlei";
pub const CHANNEL_NAME: &str = "kuku_web_genflowpro_v1";
pub const WEB_QUERY: &str = "clienttype=400&app_id=123971023&web=1&channel=chunlei&version=1.4.4";
pub const BALANCE_QUERY: &str = "channel=chunlei&clienttype=400&version=1.4.4";
pub const USER_REPORT: &str = "/api/genflowpro/common/userreport";
pub const MODELS: &str = "/wenchain/genflowpro/model/list";
pub const SEND: &str = "/wenchain/genflowpro/sendmsg";
pub const ALLOCATE: &str = "/wenchain/genflow/idallochstr";
pub const SWITCH: &str = "/api/genflowpro/workspace/sessionswitch";
pub const STREAM: &str = "/wenchain/genflowpro/sse/getchatcontent";
pub const BALANCE: &str = "/bizapi/gfpro/getgfvipremain";
pub const ACTIVITY_CONF: &str = "/act/api/conf?conf_key=kukuSignConf";
pub const ACTIVITY_PAGE: &str = "https://kuku.baidu.com/operation/activitys/kukuPcSignAct/main";
pub const ACTIVITY_TOKEN: &str = "/operation/activitys/api/getJsToken";
pub const ACTIVITY_HOME: &str = "/act/v2/kukuai30/homepage";
pub const ACTIVITY_SIGN: &str = "/act/v2/kukuai30/signin";
pub const ACTIVITY_AID: i64 = 15459;
pub const FREE_HOME: &str = "/api/genflowpro/freepoint/homenew";
pub const FREE_TASK: &str = "/api/genflowpro/freepoint/taskComplete";
pub const FREE_REWARD: &str = "/api/genflowpro/freepoint/rewardClaim";
pub const THINK_MODE: i64 = 3;
// 这是原生抓包的协议兼容UA，不是运行机器探测结果；不新增读取环境或假设备标识。
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36";
pub const ENGINE_UA: &str = "genflow;1.6.5;PC;PC-Windows;10.0.26200;GenFlowPro";
