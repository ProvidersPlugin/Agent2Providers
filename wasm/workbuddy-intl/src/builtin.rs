//! 国际目录只用本家/v3/config；原生共用的国内静态大清单不能作为国际授权依据。
//! auto来自原生默认模型契约。其余条目等国际账号在线目录，不凭模型名补图片能力。
use agent2api_wasm_sdk::{json, Value};

pub fn builtin_filtered() -> Vec<Value> {
    vec![
        json!({ "id": "auto", "name": "Auto", "isDefault": true, "modelType": "auto",
        "maxOutputTokens": 32000, "maxInputTokens": 168000, "supportsToolCall": true, "supportsImages": true }),
    ]
}
pub fn is_chat_model(model: &Value) -> bool {
    let Some(id) = model["id"].as_str() else {
        return false;
    };
    if [
        "completion-",
        "codewise-",
        "hunyuan-image",
        "hunyuan-3b",
        "hunyuan-7b",
        "deepseek-r1-0528",
        "deepseek-v3-0324",
        "kimi-k2-instruct",
        "default-1.",
    ]
    .iter()
    .any(|prefix| id.starts_with(prefix))
    {
        return false;
    }
    if model.get("supportsExtra").is_some_and(crate::util::truth) {
        return false;
    }
    model["maxOutputTokens"]
        .as_f64()
        .is_some_and(|max| max >= 16_000.0)
}
