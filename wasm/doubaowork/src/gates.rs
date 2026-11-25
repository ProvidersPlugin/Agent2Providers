use agent2api_wasm_sdk::{json,Error,Value};

pub const SUMMARY:&str="豆包工作已接通对话、思考、客户端工具桥、图片输入和生图、网页登录、本机快照导入、模型目录、额度、每日签到只读状态查询与已完成任务领奖。";
pub const CHAT:&str="对话及后续异步回答按本次原回执接收；取消只申请停止本次对话和已观察的任务，不自动重发。";
pub const LEGACY:&str="此账号是旧版实时关联，缺少 Cookie 快照；请重新导入本机登录态或完成网页登录，导入后无需保持客户端运行。";
pub const ACTIVITY:&str="任务领奖只领取官网明确完成且可领的条目，不代做任务、不购买套餐或同意付费协议；未知结果不重复领取。";
pub fn legacy()->Error{Error::http(400,LEGACY).with_code("doubaowork_legacy_reference")}
pub fn activity()->Error{Error::http(400,ACTIVITY).with_code("unsupported")}
pub fn refresh()->Error{Error::http(400,"豆包 Cookie 没有已证实的自动续期接口，请重新登录或导入登录态").with_code("doubaowork_refresh_unavailable")}
pub fn status()->Value{
    json!({"provider":"doubaowork","stage":"complete","complete":true,"verified":false,"message":SUMMARY,
        "implemented":["cookie_snapshot","online_identity","account_bound_catalog","quota_windows","activity_status","conversation","async_reply",
            "client_tool_bridge","image_upload","image_generation","browser_cookie_login","authorized_desktop_snapshot","daily_checkin_readonly","host_managed_task_rewards"],
        "blocked":{"daily_checkin_submit":"豆包工作每日签到已锁定官方桌面端安全校验，接口代签会消耗当日资格，请打开豆包工作桌面端手动签到"},
        "verification":"real_account_not_verified","conversation":CHAT,
        "network":{"singleConversationSubmission":true,"remoteIdsCaptured":true,"cancelCleanup":true,"implicitDesktopRead":false,"dynamicUpload":"host_fixed_official_domains"},
        "desktop":{"semantics":"imported_snapshot","requiresExplicitPermission":true},
        "tools":{"clientSide":"emulated","localDesktopExecution":false},"images":{"input":true,"generation":true,"editing":false}})
}
