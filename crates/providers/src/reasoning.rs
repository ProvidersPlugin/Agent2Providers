//! 思考档位的**通用口径**（插件侧）。
//!
//! ── 为什么插件要自带一份 ────────────────────────────────────
//! 「哪些等级可以翻译给上游」「客户端是不是已经显式指定了档位」是**跨家共用**
//! 的规则（网关的 `core::model_rules::reasoning` 里有同一份）。插件进程不认识
//! 网关的配置层，所以这份规则只能跟着插件走 —— 但口径必须与网关逐字一致，
//! 否则同一份映射在搬家的前后会给出不同的注入结果。
//!
//! ── 表为什么是这八个值 ──────────────────────────────────────
//! 它们是网关「思考等级」下拉的全部可选项（由弱到强，`off` 在最前）。
//! `off` / `none` 在本项目**一律不注入**（没有安全的表达方式）：调用方据此把
//! 「关闭思考」与「表外值」分开措辞 —— 两者都不注入，但用户看到的原因不同。

/// 思考等级的通用候选表（由弱到强，`off` 在最前）。
///
/// 插件侧目前没有消费方（界面下拉的候选表在网关侧），留着是为了让
/// 「哪些值算档位」与网关的 `model_rules::reasoning::REASONING_LEVELS`
/// 有一份可对照的副本 —— 将来插件自述「我认哪些档位」时会用到。
#[allow(dead_code)]
pub const REASONING_LEVELS: &[&str] =
    &["off", "none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// 可以翻译给上游的正向档位（不含 `off` / `none`）。
///
/// 数组顺序 = 强弱顺序，[`effort_rank`] 直接拿它当下标用。
pub const EFFORT_LEVELS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];

/// 等级字符串的长度上限（与网关同口径）
const REASONING_MAX_CHARS: usize = 32;

/// 归一化一个思考等级：去首尾空白 → 过长或空串返回 None（= 不覆盖）。
pub fn normalize(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.chars().count() > REASONING_MAX_CHARS {
        return None;
    }
    Some(trimmed.to_string())
}

/// 该等级是不是「关闭思考」两档（`off` / `none`），忽略大小写。
pub fn is_thinking_off(level: &str) -> bool {
    let trimmed = level.trim();
    trimmed.eq_ignore_ascii_case("off") || trimmed.eq_ignore_ascii_case("none")
}

/// 正向档位 → 序号（0 = 最弱，5 = 最强）。`off` / `none` / 表外值一律 None。
///
/// 返回 None 的含义是「**这个值不该被注入上游**」，不是「这个值不合法」。
pub fn effort_rank(level: &str) -> Option<usize> {
    let trimmed = level.trim();
    if trimmed.is_empty() {
        return None;
    }
    EFFORT_LEVELS
        .iter()
        .position(|known| trimmed.eq_ignore_ascii_case(known))
}

/// 从**下游请求体**里读客户端显式指定的思考等级。
///
/// 取值链是各家的**并集**（`reasoning_effort` → `reasoningEffort` → `effort`
/// → `reasoning`（对象形态取 `effort` 子键）→ `thinking`）。它只服务
/// 「绑定让位」这一个判据：客户端写了档位，映射上的默认值就不覆盖它。
pub fn read_client_level(body: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    for key in ["reasoning_effort", "reasoningEffort", "effort", "reasoning", "thinking"] {
        match body.get(key) {
            Some(Value::String(text)) => return normalize(text),
            // responses API 的对象形态：reasoning: { effort: "high" }
            Some(Value::Object(object)) if key == "reasoning" => {
                if let Some(Value::String(effort)) = object.get("effort") {
                    if let Some(level) = normalize(effort) {
                        return Some(level);
                    }
                }
            }
            // null / 布尔 / 数字 / 其它对象不构成「档位」，继续后面的键
            _ => {}
        }
    }
    None
}
