//! Gemini 历史工具调用的签名：客户端真实签名优先，缺失时补官方哨兵。
//! 响应缓存由网关进程持有，网关可按同一次调用的 id 用真实签名替换哨兵。

use serde_json::{Map, Value};

pub(super) const SKIP_SIGNATURE_SENTINEL: &str = "skip_thought_signature_validator";

pub(super) fn call_signature(call: &Value) -> Option<String> {
    [
        "/extra_content/google/thought_signature",
        "/thought_signature",
        "/thoughtSignature",
    ].iter().find_map(|pointer| {
        call.pointer(pointer).and_then(Value::as_str).map(str::trim)
            .filter(|value| !value.is_empty()).map(str::to_string)
    })
}

pub(super) fn fill_missing(contents: &mut [Value], model: &str) -> usize {
    let model = model.to_ascii_lowercase();
    if !model.starts_with("gemini-") && !model.starts_with("gpt-oss-") {
        return 0;
    }
    let mut filled = 0;
    for turn in contents.iter_mut().filter(|turn| turn.get("role").and_then(Value::as_str) == Some("model")) {
        let Some(parts) = turn.get_mut("parts").and_then(Value::as_array_mut) else { continue };
        for part in parts.iter_mut().filter_map(Value::as_object_mut) {
            if part.get("functionCall").is_some_and(Value::is_object) && signature_missing(part) {
                part.insert("thoughtSignature".to_string(), Value::String(SKIP_SIGNATURE_SENTINEL.to_string()));
                filled += 1;
            }
        }
    }
    filled
}

fn signature_missing(part: &Map<String, Value>) -> bool {
    part.get("thoughtSignature").and_then(Value::as_str)
        .is_none_or(|value| value.trim().is_empty())
}
