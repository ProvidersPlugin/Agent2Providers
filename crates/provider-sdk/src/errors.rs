//! 控制面错误在 String 回调与 RPC 之间保留 HTTP 状态和业务码。

use agent2api_provider_protocol as proto;
use serde::{Deserialize, Serialize};

const CONTROL_ERROR_PREFIX: &str = "CONTROL_ERROR:";
const HTTP_CODE_PREFIX: &str = "provider_http:";

#[derive(Serialize, Deserialize)]
struct ControlError {
    message: String,
    #[serde(flatten)]
    metadata: ErrorMetadata,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ErrorMetadata {
    status: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    upstream_code: Option<i64>,
}

pub fn control_error(
    status: i32,
    message: impl Into<String>,
    code: Option<String>,
    upstream_code: Option<i64>,
) -> String {
    let error = ControlError {
        message: message.into(),
        metadata: ErrorMetadata { status, code, upstream_code },
    };
    if !(400..600).contains(&status) {
        return error.message;
    }
    match serde_json::to_string(&error) {
        Ok(payload) => format!("{CONTROL_ERROR_PREFIX}{payload}"),
        Err(_) => error.message,
    }
}

pub(crate) fn split_control_error(message: &str) -> Option<proto::WireError> {
    let payload = message.strip_prefix(CONTROL_ERROR_PREFIX)?;
    let error: ControlError = serde_json::from_str(payload).ok()?;
    if !(400..600).contains(&error.metadata.status) {
        return None;
    }
    let metadata = serde_json::to_string(&error.metadata).ok()?;
    Some(proto::WireError::with_code(error.message, format!("{HTTP_CODE_PREFIX}{metadata}")))
}
