//! 思考等级后缀剥离、档位排序、默认档位选取与上游 thinkingLevel 映射。
//! 目录合并、配额聚合、请求路由共用这一份口径，避免两处展示或路由分叉。

use agent2api_wasm_sdk::Value;

/// 与网关 `core::model_rules::reasoning::EFFORT_LEVELS` 一致的弱→强标准档位序。
pub const LEVEL_ORDER: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// 识别并剥离的 ID 后缀（长后缀在前，避免 `extra-low` 只剥掉 `low`）。
const ID_SUFFIXES: [(&str, &str); 9] = [
    ("extra-low", "minimal"),
    ("extra_low", "minimal"),
    ("minimal", "minimal"),
    ("xlow", "minimal"),
    ("xhigh", "xhigh"),
    ("medium", "medium"),
    ("high", "high"),
    ("low", "low"),
    ("max", "max"),
];

/// 展示名括号或末尾词识别表（包含带空格的 `extra low`）。
const NAME_TOKENS: [&str; 9] = [
    "extra-low",
    "extra_low",
    "extra low",
    "minimal",
    "xhigh",
    "xlow",
    "medium",
    "high",
    "low",
];

/// 把各种客户端或后缀写法归一到标准 6 档之一。
pub fn normalize_level(raw: &str) -> Option<&'static str> {
    let cleaned = raw.trim().to_ascii_lowercase().replace('_', "-");
    match cleaned.as_str() {
        "minimal" | "min" | "extra-low" | "xlow" => Some("minimal"),
        "low" => Some("low"),
        "medium" | "med" | "normal" | "standard" => Some("medium"),
        "high" => Some("high"),
        "xhigh" | "x-high" | "extra-high" | "extra" => Some("xhigh"),
        "max" | "extreme" => Some("max"),
        _ => None,
    }
}

/// 解析上游 `fetchAvailableModels` 里的 `thinkingLevel`（支持字符串与数字 1/2/3/4）。
pub fn parse_upstream_thinking_level(value: &Value) -> Option<&'static str> {
    if let Some(n) = value.as_i64() {
        return match n {
            1 => Some("low"),
            2 => Some("medium"),
            3 => Some("high"),
            4 => Some("xhigh"),
            _ => None,
        };
    }
    let text = value.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    match text {
        "1" => Some("low"),
        "2" => Some("medium"),
        "3" => Some("high"),
        "4" => Some("xhigh"),
        other => normalize_level(other),
    }
}

/// 将标准档位转换为上游 `thinkingConfig.thinkingLevel` 枚举值。
/// Gemini 4+ 拒绝 `MINIMAL`，最低下发 `LOW`；`xhigh`/`max` 下发 `XHIGH`。
pub fn to_upstream_thinking_level(level: &str) -> &'static str {
    match normalize_level(level).unwrap_or("medium") {
        "minimal" | "low" => "LOW",
        "medium" => "MEDIUM",
        "high" => "HIGH",
        "xhigh" | "max" => "XHIGH",
        _ => "MEDIUM",
    }
}

/// 提取上游原始条目里的 `thinkingLevel` 枚举文本（保留字符串形态或由数字映射）。
pub fn upstream_thinking_level_wire(info: &Value) -> Option<String> {
    let raw = info
        .get("thinkingLevel")
        .or_else(|| info.get("thinking_level"))?;
    if raw.is_null() {
        return None;
    }
    if let Some(level) = parse_upstream_thinking_level(raw) {
        return Some(to_upstream_thinking_level(level).to_string());
    }
    let text = raw.as_str()?.trim();
    (!text.is_empty() && !text.chars().any(char::is_control)).then(|| text.to_ascii_uppercase())
}

/// 剥离模型 ID 末尾的一层思考等级后缀，返回 `(基础ID, 归一化标准档位)`。
/// 不会剥离 `-tiered`、`-sonar`、`-lite`、`-thinking` 等非思考等级后缀。
pub fn strip_effort_id(id: &str, allow_max: bool) -> (&str, Option<&'static str>) {
    let lower = id.to_ascii_lowercase();
    for (token, standard) in ID_SUFFIXES {
        if token == "max" && !allow_max {
            continue;
        }
        for sep in ['-', '_'] {
            if lower.len() > token.len() + 1
                && lower.ends_with(token)
                && lower.as_bytes()[lower.len() - token.len() - 1] == sep as u8
            {
                let base = id[..id.len() - token.len() - 1].trim_end_matches(['-', '_']);
                if !base.is_empty() {
                    return (base, Some(standard));
                }
            }
        }
    }
    (id, None)
}

/// 剥离展示名末尾括号内的思考等级标注，如 `"Claude Opus 5.5 (High)"` → `"Claude Opus 5.5"`。
pub fn strip_parenthesized_effort(name: &str) -> (&str, bool) {
    let trimmed = name.trim();
    let open = if trimmed.ends_with(')') {
        trimmed.rfind('(')
    } else if trimmed.ends_with('）') {
        trimmed.rfind('（')
    } else {
        return (trimmed, false);
    };
    let Some(pos) = open else {
        return (trimmed, false);
    };
    let close_len = if trimmed.ends_with('）') {
        '）'.len_utf8()
    } else {
        1
    };
    let open_len = if trimmed[pos..].starts_with('（') {
        '（'.len_utf8()
    } else {
        1
    };
    let inner = trimmed[pos + open_len..trimmed.len() - close_len]
        .trim()
        .to_ascii_lowercase();
    let is_effort = inner == "max" || NAME_TOKENS.iter().any(|tok| *tok == inner);
    if !is_effort {
        return (trimmed, false);
    }
    let prefix = trimmed[..pos].trim_end();
    if prefix.is_empty() {
        (trimmed, false)
    } else {
        (prefix, true)
    }
}

/// 剥离展示名末尾括号或空格/连字符分隔的思考等级词。
pub fn strip_effort_name(name: &str, allow_max: bool) -> (&str, bool) {
    let (without_paren, had_paren) = strip_parenthesized_effort(name);
    if had_paren {
        return (without_paren, true);
    }
    let lower = without_paren.to_ascii_lowercase();
    let candidates = NAME_TOKENS
        .iter()
        .copied()
        .chain(allow_max.then_some("max"));
    for token in candidates {
        if lower.len() > token.len() + 1 && lower.ends_with(token) {
            let boundary = lower.as_bytes()[lower.len() - token.len() - 1];
            if boundary == b' ' || boundary == b'-' {
                let prefix = without_paren[..without_paren.len() - token.len()]
                    .trim_end_matches([' ', '-']);
                if !prefix.is_empty() {
                    return (prefix, true);
                }
            }
        }
    }
    (without_paren, false)
}

pub fn level_rank(level: &str) -> usize {
    let target = normalize_level(level).unwrap_or(level);
    LEVEL_ORDER
        .iter()
        .position(|known| known.eq_ignore_ascii_case(target))
        .unwrap_or(usize::MAX)
}

/// 按弱→强顺序去重排列档位列表。
pub fn sort_levels(levels: &mut Vec<String>) {
    levels.sort_by(|a, b| level_rank(a).cmp(&level_rank(b)).then_with(|| a.cmp(b)));
    levels.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
}

/// 选取默认档位：
/// 1. 若上游无后缀本体声明了有效档位且在可用列表中，优先采用；
/// 2. 否则优先 `medium`；
/// 3. 否则取高于 `low` 的最低档位（如 `[low, high]` → `high`，`[high, xhigh]` → `high`）；
/// 4. 否则有 `low` 取 `low`，再否则取首项。
pub fn pick_default_level(levels: &[String], bare_hint: Option<&str>) -> Option<String> {
    if levels.is_empty() {
        return None;
    }
    if let Some(hint) = bare_hint.and_then(normalize_level) {
        if let Some(found) = levels.iter().find(|item| item.eq_ignore_ascii_case(hint)) {
            return Some(found.clone());
        }
    }
    if let Some(found) = levels.iter().find(|item| item.eq_ignore_ascii_case("medium")) {
        return Some(found.clone());
    }
    let low_rank = level_rank("low");
    if let Some(found) = levels
        .iter()
        .filter(|item| level_rank(item) > low_rank && level_rank(item) < usize::MAX)
        .min_by_key(|item| level_rank(item))
    {
        return Some(found.clone());
    }
    if let Some(found) = levels.iter().find(|item| item.eq_ignore_ascii_case("low")) {
        return Some(found.clone());
    }
    levels.first().cloned()
}

/// 在可用档位中匹配请求档位：精确命中优先，否则按强弱序就近匹配（等距取较高档）。
pub fn pick_closest_level(levels: &[String], requested: &str) -> Option<String> {
    let trimmed = requested.trim();
    if trimmed.is_empty() || levels.is_empty() {
        return None;
    }
    if let Some(exact) = levels.iter().find(|item| item.eq_ignore_ascii_case(trimmed)) {
        return Some(exact.clone());
    }
    let norm = normalize_level(trimmed)?;
    if let Some(exact) = levels.iter().find(|item| item.eq_ignore_ascii_case(norm)) {
        return Some(exact.clone());
    }
    let target_rank = level_rank(norm);
    if target_rank == usize::MAX {
        return None;
    }
    levels
        .iter()
        .filter_map(|item| {
            let rank = level_rank(item);
            (rank < usize::MAX).then_some((rank.abs_diff(target_rank), rank, item))
        })
        // 距离越小越优先；同距离时选较高档（Reverse(rank)）。
        .min_by_key(|(dist, rank, _)| (*dist, std::cmp::Reverse(*rank)))
        .map(|(_, _, item)| item.clone())
}

/// 判定某上游模型是否采用新一代 `thinkingLevel` 协议（严禁下发 `thinkingBudget`）。
pub fn uses_thinking_level(id: &str, info: Option<&Value>) -> bool {
    let lower = id.trim().to_ascii_lowercase();
    // Gemini 2.x 维持旧预算逻辑，避免受目录测试标记干扰。
    if lower.starts_with("gemini-2.") {
        return false;
    }
    if let Some(info) = info {
        if info["antigravityThinkingMode"].as_str() == Some("level")
            || info["thinkingMode"].as_str() == Some("level")
        {
            return true;
        }
        if info["antigravityThinkingMode"].as_str() == Some("budget")
            || info["thinkingMode"].as_str() == Some("budget")
        {
            return false;
        }
        if upstream_thinking_level_wire(info).is_some() {
            return true;
        }
        if info
            .get("thinkingBudget")
            .or_else(|| info.get("thinking_budget"))
            .and_then(Value::as_i64)
            .is_some_and(|n| n > 0)
        {
            return false;
        }
    }
    if lower.starts_with("gemini-4") || lower.contains("argon") {
        return true;
    }
    if lower.starts_with("claude-") {
        if let Some((major, _)) = super::catalog_shape::claude_version(&lower) {
            return major >= 5;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_preserves_tiered_and_sonar_while_splitting_effort() {
        assert_eq!(
            strip_effort_id("gemini-4-hydrogen-tiered-high", false),
            ("gemini-4-hydrogen-tiered", Some("high"))
        );
        assert_eq!(
            strip_effort_id("gemini-4-hydrogen-tiered-xhigh", false),
            ("gemini-4-hydrogen-tiered", Some("xhigh"))
        );
        assert_eq!(
            strip_effort_id("gemini-4-hydrogen-sonar", false),
            ("gemini-4-hydrogen-sonar", None)
        );
        assert_eq!(
            strip_effort_id("gemini-4-hydrogen-xhigh", false),
            ("gemini-4-hydrogen", Some("xhigh"))
        );
        assert_eq!(
            strip_effort_id("claude-opus-5-5-medium", false),
            ("claude-opus-5-5", Some("medium"))
        );
        assert_eq!(
            strip_effort_id("gemini-3.5-flash-extra-low", false),
            ("gemini-3.5-flash", Some("minimal"))
        );
        assert_eq!(
            strip_effort_id("claude-opus-4-6-thinking", false),
            ("claude-opus-4-6-thinking", None)
        );
        assert_eq!(
            strip_effort_id("gpt-oss-120b-medium", false),
            ("gpt-oss-120b", Some("medium"))
        );
    }

    #[test]
    fn default_and_closest_levels_follow_priority_rules() {
        let three: Vec<String> = ["low", "medium", "high"].iter().map(|s| s.to_string()).collect();
        assert_eq!(pick_default_level(&three, None).as_deref(), Some("medium"));
        assert_eq!(pick_closest_level(&three, "xhigh").as_deref(), Some("high"));
        assert_eq!(pick_closest_level(&three, "minimal").as_deref(), Some("low"));

        let tiered: Vec<String> = ["high", "xhigh"].iter().map(|s| s.to_string()).collect();
        assert_eq!(pick_default_level(&tiered, None).as_deref(), Some("high"));
        assert_eq!(pick_closest_level(&tiered, "low").as_deref(), Some("high"));
        assert_eq!(pick_closest_level(&tiered, "max").as_deref(), Some("xhigh"));

        let pro: Vec<String> = ["low", "high"].iter().map(|s| s.to_string()).collect();
        assert_eq!(pick_default_level(&pro, None).as_deref(), Some("high"));
        assert_eq!(pick_closest_level(&pro, "medium").as_deref(), Some("high"));
    }
}
