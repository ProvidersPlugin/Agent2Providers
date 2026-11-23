//! 国际桌面导入严格保持原生一次性快照语义；后续不再读取客户端登录文件。
//! helper仅允许Windows LOCALAPPDATA/CodeBuddyExtension/Data/Public/auth/workbuddy-desktop-ai.info，
//! macOS ~/Library/Application Support/CodeBuddyExtension/Data/Public/auth/同名文件；
//! Linux ~/.local/share/CodeBuddyExtension/Data/Public/auth/同名文件。
//! 当前文件缺失才允许workbuddy-desktop-ai.<时间戳>.<pid>.<uuid>.info备份，拒绝国内文件。
//! 提供者须拒绝符号链接/非普通文件/空文件/>256KiB，不接受任意路径；明文JSON无需SQLite/DPAPI。
use crate::endpoints;
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub fn read() -> Result<Value> {
    let value = sdk::native_call(endpoints::HELPER, "read", json!({})).map_err(|_| {
        Error::http(
            503,
            "WorkBuddy 国际版本机登录态不可读，请先在官方国际版客户端登录，或改用官方授权和 JSON 导入",
        )
        .with_code("desktop_helper_unavailable")
    })?;
    let content = value["content"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= endpoints::MAX_CREDENTIALS)
        .ok_or_else(|| Error::http(502, "国际版本机辅助未返回有效凭据文本"))?;
    for key in ["sourceId", "sourceRevision"] {
        if !value[key]
            .as_str()
            .is_some_and(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        {
            return Err(Error::http(502, "国际版本机辅助缺少来源信息"));
        }
    }
    serde_json::from_str(content).map_err(|_| Error::http(502, "国际版本机登录态不是有效JSON"))
}
