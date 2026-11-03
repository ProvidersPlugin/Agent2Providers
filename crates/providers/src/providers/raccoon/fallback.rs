//! 2026-11-01 的认证 model_catalog 快照；远程目录始终优先。

use serde_json::{json, Value};

pub(super) fn models() -> Vec<Value> {
    let mut models: Vec<Value> = [
        (
            "raccoon-8c4485",
            "Raccoon-Work",
            1_000_000,
            100_000,
            true,
            1.0,
            3,
        ),
        (
            "raccoon-19b265",
            "Raccoon-Work-260817-A",
            1_000_000,
            100_000,
            false,
            1.0,
            3,
        ),
        (
            "raccoon-405a1c",
            "Raccoon-Work-260817-B",
            1_000_000,
            100_000,
            false,
            1.0,
            2,
        ),
        (
            "sn-sensenova-6-8-flash",
            "SenseNova-6.8-Flash",
            256_000,
            63_999,
            true,
            0.5,
            1,
        ),
        (
            "sn-sensenova-6-8-flash-lite",
            "SenseNova-6.8-Flash-Lite",
            256_000,
            63_999,
            true,
            0.5,
            1,
        ),
        ("sn-glm-5-3", "GLM-5-3", 1_000_000, 100_000, false, 0.75, 2),
        ("sn-kimi-k3", "Kimi-K3", 1_000_000, 100_000, true, 1.0, 2),
        (
            "sn-glm-5-3-flash",
            "GLM-5-3-Flash",
            1_000_000,
            100_000,
            true,
            0.2,
            2,
        ),
        (
            "sn-deepseek-v4-1-flash-1017",
            "DeepSeek-V4.1-Flash-1017",
            1_000_000,
            100_000,
            true,
            0.25,
            3,
        ),
    ]
    .into_iter()
    .map(
        |(id, display, context, output, vision, multiplier, ability)| {
            json!({
                "name": id,
                "displayName": display,
                "description": "",
                "contextWindow": context,
                "maxTokens": output,
                "supportImage": vision,
                "supportThinking": super::reasoning::supported(id),
                "abilityLevel": ability,
                "visible": !id.starts_with("raccoon-"),
                "visibility": if id.starts_with("raccoon-") { "hidden" } else { "shown" },
                "credits": format!("x{multiplier} credits"),
            })
        },
    )
    .collect();
    let default = super::models::default_model_id();
    if !models
        .iter()
        .any(|model| model.get("name").and_then(Value::as_str) == Some(default.as_str()))
    {
        models.push(json!({
            "name": default,
            "displayName": "Raccoon Custom Default",
            "supportThinking": super::reasoning::supported(&default),
        }));
    }
    models
}
