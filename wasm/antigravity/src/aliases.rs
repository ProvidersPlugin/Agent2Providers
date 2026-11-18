//! 原生 models.rs 的21条精确映射与裸 Flash 规则，别名不等于目录广告或账号资格。

pub const ALIASES: [(&str, &str); 21] = [
    ("gemini-3.6-flash", "gemini-3.6-flash-tiered"),
    ("gemini-3.7-flash", "gemini-3.7-flash-tiered"),
    ("gemini-3.8-flash", "gemini-3.8-flash-tiered"),
    ("gemini-3.1-pro-high", "gemini-pro-agent"),
    ("gemini-3-pro-high", "gemini-pro-agent"),
    ("gemini-2.5-flash-lite", "gemini-2.5-flash"),
    ("claude-sonnet-4-5", "claude-sonnet-4-6"),
    ("claude-sonnet-4-5-thinking", "claude-sonnet-4-6-thinking"),
    ("claude-sonnet-4-5-20250929", "claude-sonnet-4-6-thinking"),
    ("claude-opus-4", "claude-opus-4-6-thinking"),
    ("claude-opus-4-5-thinking", "claude-opus-4-6-thinking"),
    ("claude-opus-4-5-20251101", "claude-opus-4-6-thinking"),
    ("claude-opus-4-6", "claude-opus-4-6-thinking"),
    ("claude-opus-4.6", "claude-opus-4-6-thinking"),
    ("claude-opus-4.6-thinking", "claude-opus-4-6-thinking"),
    ("claude-opus-4-6-20260201", "claude-opus-4-6-thinking"),
    ("claude-3-5-sonnet-20241022", "claude-sonnet-4-6"),
    ("claude-3-5-sonnet-20240620", "claude-sonnet-4-6"),
    ("claude-haiku-4", "claude-sonnet-4-6"),
    ("claude-haiku-4-5-20251001", "claude-sonnet-4-6"),
    ("claude-3-haiku-20240307", "claude-sonnet-4-6"),
];

pub fn for_catalog(requested: &str, list: &[agent2api_wasm_sdk::Value]) -> String {
    let direct = list.iter().find_map(|entry| {
        entry["id"]
            .as_str()
            .filter(|id| id.eq_ignore_ascii_case(requested.trim()))
    });
    if let Some(id) = direct {
        if id.eq_ignore_ascii_case("gemini-3.1-pro-high")
            || id.eq_ignore_ascii_case("gemini-pro-agent")
        {
            return id.to_string();
        }
    }
    let legacy = upstream(requested);
    let current = if legacy == "gemini-pro-agent" {
        "gemini-3.1-pro-high"
    } else if legacy == "gemini-3.1-pro-high" {
        "gemini-pro-agent"
    } else {
        ""
    };
    if !current.is_empty()
        && !list.iter().any(|entry| entry["id"] == legacy)
        && list.iter().any(|entry| entry["id"] == current)
    {
        return current.to_string();
    }
    legacy
}

pub fn upstream(id: &str) -> String {
    let trimmed = id.trim();
    if let Some(id) = super::argon::resolve_model(trimmed, None) {
        return id.into();
    }
    let lowered = trimmed.to_ascii_lowercase();
    for (alias, target) in ALIASES {
        if lowered == alias {
            return target.into();
        }
    }
    if let Some(tiered) = tiered(trimmed, &lowered) {
        return tiered;
    }
    trimmed.into()
}

fn tiered(original: &str, lowered: &str) -> Option<String> {
    if [
        "-high",
        "-medium",
        "-low",
        "-extra-low",
        "-tiered",
        "-preview",
        "-agent",
        "-thinking",
        "-image",
    ]
    .iter()
    .any(|suffix| lowered.contains(suffix))
        || !lowered.contains("flash")
    {
        return None;
    }
    let version: String = lowered
        .strip_prefix("gemini-")?
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect();
    if version.trim_end_matches('.').parse::<f64>().ok()? < 3.6 {
        return None;
    }
    Some(format!("{original}-tiered"))
}
