//! 一次性快照导入。只有account.normalize显式desktop来源可以调用此helper。
//! Windows LOCALAPPDATA/CodeBuddyExtension/Data/Public/auth/workbuddy-desktop.info；
//! macOS ~/Library/Application Support/CodeBuddyExtension/Data/Public/auth/同名文件，
//! Linux ~/.local/share/CodeBuddyExtension/Data/Public/auth/同名文件。
//! 当前文件缺失时仅允许workbuddy-desktop.<时间戳>.<pid>.<uuid>.info备份；不能匹配-ai。
//! helper须拒绝符号链接/非普通文件/空文件/>256KiB；不接受路径参数。
//! 明文JSON可投影；客户端加密信封明确拒绝，不新增系统解密或进程执行。
use super::endpoints;
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub fn read() -> Result<Value> {
    let value = sdk::native_call(endpoints::HELPER, "read", json!({})).map_err(|mut error| {
        error.status = Some(503);
        error.message =
            "WorkBuddy 本机登录态不可读，请先在官方客户端登录，或改用网页登录和 JSON 导入".into();
        error
    })?;
    let content = value["content"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= endpoints::MAX_CREDENTIALS)
        .ok_or_else(|| Error::http(502, "WorkBuddy本机辅助未返回有效凭据文本"))?;
    // 来源只用于核对helper契约，不需要把本机路径或版本写入账号。
    for key in ["sourceId", "sourceRevision"] {
        if !value[key]
            .as_str()
            .is_some_and(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        {
            return Err(Error::http(502, "WorkBuddy本机辅助缺少来源信息"));
        }
    }
    serde_json::from_str(content).map_err(|_| Error::http(502, "WorkBuddy本机登录态不是有效JSON"))
}
