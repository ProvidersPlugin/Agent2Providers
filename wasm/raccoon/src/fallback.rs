//! 原生2026-11-01认证目录快照；只作为冷启动兜底，不把限免写成永久倍率。

use agent2api_wasm_sdk::{json, Value};

pub fn models() -> Vec<Value> {
    let mut values: Vec<Value> = [
        ("raccoon-8c4485", "Raccoon-Work", 1_000_000, 100_000, true, 1.0, 3),
        ("raccoon-19b265", "Raccoon-Work-260817-A", 1_000_000, 100_000, false, 1.0, 3),
        ("raccoon-405a1c", "Raccoon-Work-260817-B", 1_000_000, 100_000, false, 1.0, 2),
        ("sn-sensenova-6-8-flash", "SenseNova-6.8-Flash", 256_000, 63_999, true, 0.5, 1),
        ("sn-sensenova-6-8-flash-lite", "SenseNova-6.8-Flash-Lite", 256_000, 63_999, true, 0.5, 1),
        ("sn-glm-5-3", "GLM-5-3", 1_000_000, 100_000, false, 0.75, 2),
        ("sn-kimi-k3", "Kimi-K3", 1_000_000, 100_000, true, 1.0, 2),
        ("sn-glm-5-3-flash", "GLM-5-3-Flash", 1_000_000, 100_000, true, 0.2, 2),
        ("sn-deepseek-v4-1-flash-1017", "DeepSeek-V4.1-Flash-1017", 1_000_000, 100_000, true, 0.25, 3),
    ].into_iter().map(|(id, name, context, output, vision, rate, ability)| json!({
        "id": id, "name": name, "displayName": name, "description": "", "maxInputTokens": context,
        "contextWindow": context, "maxOutputTokens": output, "maxTokens": output,
        "supportsImages": vision, "supportImage": vision, "supportsToolCall": id == "sn-glm-5-3",
        "supportsReasoning": super::reasoning::supported(id),
        "supportThinking": super::reasoning::supported(id), "abilityLevel": ability,
        "visible": !id.starts_with("raccoon-"), "visibility": if id.starts_with("raccoon-") { "hidden" } else { "shown" },
        "credits": format!("x{rate} credits")
    })).collect();
    values.push(super::images::model());
    values
}
