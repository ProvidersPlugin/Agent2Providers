use agent2api_wasm_sdk::{json, Error, Value};

pub const SUMMARY:&str="MonkeyCode 国际版已接通独立账号校验、目录、钱包、流水和订阅、受控远程对话与客户端工具桥、网页登录和每日签到。";
pub fn web_login() -> Error {
    Error::http(
        400,
        "国际版登录请使用官方网页窗口或粘贴 monkeycode-ai.net 的 Session",
    )
    .with_code("monkeycode_intl_login_method")
}
pub fn spending() -> Error {
    Error::http(
        400,
        "券码、购买和邀请不是免费每日签到，本插件不自动消费或代做这些操作",
    )
    .with_code("unsupported")
}
pub fn refresh() -> Error {
    Error::http(
        400,
        "MonkeyCode 国际 Session 没有已证实的自动续期接口，请重新登录",
    )
    .with_code("monkeycode_intl_refresh_unavailable")
}
pub fn images() -> Error {
    Error::http(
        400,
        "MonkeyCode 国际公开任务输入仅表达文本和客户端工具历史，不提供图片生成或编辑接口",
    )
    .with_code("unsupported_parameter")
}
pub fn status() -> Value {
    json!({"provider":crate::endpoints::ID,"stage":"complete","completeMigration":true,"integrationVerified":false,"liveVerified":false,"message":SUMMARY,
        "account":{"ready":true},"catalog":{"ready":true,"completeAccountPermissions":false},"wallet":{"ready":true,"readOnly":true,"unit":"以官方响应为准"},
        "chat":{"ready":true,"executionEnabled":true,"requiresExplicitGrant":false,"authorization":"current_conversation","clientToolContinuation":true},
        "web_login":{"ready":true,"embeddedOnly":true,"desktopOnly":true},"checkin":{"ready":true,"daily":true,"captcha":"official_cap_computation"},
        "refresh":{"ready":false,"reason":refresh().message},"images":{"ready":false,"reason":images().message}})
}
