//! MonkeyCode 任务流帧的 `data` 编解码（**base64(JSON)**）。
//!
//! ── 为什么单独一个文件 ───────────────────────────────────────
//! 上行封帧与下行解帧是本家**唯一**的编码约定，`translate.rs`（下行翻译）与
//! `stream.rs`（起始消息）都要用；单独放一处，避免两处各写一份 —— 历史上就是
//! 各写一份「裸 JSON 字符串」，与上游不符，导致整条链一帧都解不出。
//!
//! ── 约定从哪来（以上游为准）───────────────────────────────────
//! 上游 `domain.TaskStream.Data` 是 Go 的 `[]byte`，Go 对 `[]byte` 的 JSON
//! 编解码一律走 base64 —— 也就是**线上每个帧的 `data` 都是 base64(JSON)**：
//!   - 下行：官方网页端 `task-message-handler.ts::decodeChunkPayloadJSON`
//!     就是 `JSON.parse(b64decode(chunk.data))`；参考仓库
//!     `codkeep/MonkeyCodeReverseEngineer` 的 `docs/10-appendices/07-websocket-frames.md`
//!     里那条真实抓包也是 base64；
//!   - 上行：官方网页端 `task-stream-client.ts::sendMessage` 与桌面端
//!     `lib/cloud/stream.ts::send` 都是 `b64encode(JSON.stringify(payload))`
//!     （`stream.test.ts` 的断言直接写着「双层 base64」）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic，不碰网络、不持锁。

use base64::Engine;
use serde_json::{json, Value};

use crate::logging;

/// 下行 `data` → JSON。
///
/// 以 base64(JSON) 为主，同时兼容两种历史 / 自建形态：内联 JSON 对象、裸 JSON
/// 字符串 —— 三种都认，避免上游某个部署换了形态就整条链全断。
pub(super) fn decode_data(data: Option<&Value>) -> Option<Value> {
    match data? {
        Value::Object(_) | Value::Array(_) => data.cloned(),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return None;
            }
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(trimmed) {
                if let Ok(decoded) = String::from_utf8(bytes) {
                    if let Ok(value) = serde_json::from_str::<Value>(decoded.trim()) {
                        return Some(value);
                    }
                }
            }
            serde_json::from_str::<Value>(trimmed).ok()
        }
        _ => None,
    }
}

/// 上行帧：`{type, data: base64(JSON(payload)), timestamp}`。
///
/// 上游 `domain.TaskStream.Data` 是 `[]byte`，Go 的 JSON 编解码对 `[]byte` 就是
/// base64；官方网页端 `task-stream-client.ts::sendMessage` 与桌面端
/// `lib/cloud/stream.ts::send` 都按 `b64encode(JSON.stringify(payload))` 发。
pub(super) fn uplink_frame(frame_type: &str, payload: &Value) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(payload.to_string().as_bytes());
    json!({
        "type": frame_type,
        "data": encoded,
        "timestamp": logging::now_ms(),
    })
    .to_string()
}
