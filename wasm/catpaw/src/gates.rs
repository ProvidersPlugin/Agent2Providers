use agent2api_wasm_sdk::{json,Error,Value};

pub(super) const TURN_CONSUMER_ENABLED:bool=true;
pub const CHAT:&str="CatPaw 对话和工具续接由主程序固定轮次服务执行；原账号、完整历史、模型目录及创建回执逐次核对。";
pub const WEB_LOGIN:&str="CatPaw 网页登录使用主程序托管的官方 passport 流程，完成后核对真实用户。";
pub const DESKTOP:&str="本机实时关联只保存来源，使用时只读当前客户端登录态，无需单独配置权限，不写回一次性令牌。";
pub const REFRESH:&str="CatPaw passport 没有已证实的自动续期接口，请重新网页登录；本机实时读取不等于自动续期。";
pub const SUMMARY:&str="CatPaw 已接通对话与跨请求工具续接、图片输入和压缩、官方网页登录、本机实时关联、账号隔离的模型目录和积分查询。";
pub fn chat()->Error{Error::http(503,CHAT).with_code("catpaw_conversation_host_unavailable")}
pub fn desktop()->Error{Error::http(400,DESKTOP).with_code("catpaw_desktop_reference_only")}
pub fn refresh()->Error{Error::http(400,REFRESH).with_code("catpaw_refresh_unavailable")}
pub fn activity()->Error{Error::http(501,"没有已证实的 CatPaw 每日签到或领取接口，不执行猜测活动请求").with_code("catpaw_activity_unverified")}
pub fn status()->Value{
    json!({"provider":"catpaw","stage":"complete","complete":true,"verified":false,"message":SUMMARY,
        "sourceImplemented":["manual_snapshot","file_text_snapshot","managed_passport_login","model_catalog","credit_balance","conversation","tool_continuation","inline_images","image_compression","authorized_desktop_live"],
        "blocked":{},"verification":"real_account_not_verified",
        "conversation":{"enabled":true,"profile":crate::turn_host::PROFILE,"hostCapability":crate::turn_host::CAPABILITY,"toolContinuationEnabled":true,"ordinaryHttpFallback":false,"cancelCleanup":true},
        "desktop":{"semantics":"live_reference","enabled":true,"requiresExplicitImport":true,"requiresExplicitPermission":false,"persistToken":false},
        "models":{"accountIsolated":true,"staticFallback":"display_only","emptyCatalog":"confirmed_remote"},
        "images":{"input":true,"generation":false,"editing":false},"webLogin":{"methodIndex":0,"profile":"catpaw.passport.v1"}})
}
