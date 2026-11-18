//! 原生多档上下文选择：CJK 估算 + 15% 余量；只升到目录声明的最小可用档。
//! 三处字段同步，不改用户消息、不虚构支持长度；没有档位时保持上游默认。
use agent2api_wasm_sdk::{json, Value};
use super::util;

pub fn tiers(row: &Value) -> Vec<Value> {
    let mut output = Vec::new();
    let mut push = |key: &str, value: &Value| {
        let Some(tokens) = value.get("token_count").or_else(|| value.get("tokenCount")).and_then(Value::as_i64).filter(|tokens| *tokens > 0) else { return; };
        let name = util::text(value, &["name", "label", "key", "id"]);
        output.push(json!({ "tokens": tokens, "name": if name.is_empty() { key } else { &name },
            "isDefault": value.get("is_default").or_else(|| value.get("isDefault")).is_some_and(util::truthy) }));
    };
    match &row["context_config"] {
        Value::Object(map) => for (key, value) in map { push(key, value); },
        Value::Array(values) => for value in values { push("", value); },
        _ => {}
    }
    output.sort_by_key(|value| value["tokens"].as_i64().unwrap_or(0));
    output.dedup_by_key(|value| value["tokens"].as_i64().unwrap_or(0));
    output
}
pub fn estimate(messages: &[Value], tools: &[Value]) -> i64 {
    let raw = json!({ "messages": messages, "tools": tools }).to_string();
    let mut cjk = 0_i64;
    let mut total = 0_i64;
    for ch in raw.chars() {
        total += 1;
        if matches!(ch as u32, 0x1100..=0x11ff | 0x2e80..=0x9fff | 0xac00..=0xd7af | 0xf900..=0xfaff | 0xff00..=0xffef) { cjk += 1; }
    }
    cjk + (total - cjk + 3) / 4
}
pub fn apply(payload: &mut Value, config: &Value, messages: &[Value], tools: &[Value]) {
    let Some(tiers) = config["tiers"].as_array().filter(|tiers| !tiers.is_empty()) else { return; };
    let Some(default) = tiers.iter().find(|tier| tier["isDefault"] == true).or_else(|| tiers.first()) else { return; };
    let current = config["max_input_tokens"].as_i64().filter(|tokens| *tokens > 0).or_else(|| default["tokens"].as_i64()).unwrap_or(0);
    let need = (estimate(messages, tools) as f64 * 1.15).ceil() as i64;
    if need <= current { return; }
    let target = tiers.iter().find(|tier| tier["tokens"].as_i64().is_some_and(|tokens| tokens >= need && tokens > current))
        .or_else(|| tiers.iter().max_by_key(|tier| tier["tokens"].as_i64().unwrap_or(0)));
    let Some(tokens) = target.and_then(|target| target["tokens"].as_i64()).filter(|tokens| *tokens > current) else { return; };
    payload["parameters"]["context_length"] = json!(tokens);
    payload["model_config"]["max_input_tokens"] = json!(tokens);
    payload["chat_context"]["extra"]["ideModelConfigOverride"] = json!({ "max_input_tokens": tokens });
}
