//! 思考等级的正向档位表（网关 `core::model_rules::reasoning_rank` 的插件侧等价物）。
//!
//! ── 为什么插件里要有一份 ────────────────────────────────────
//! 搬来的家（Qoder / Accio / Antigravity）用 `reasoning_rank(level)` 判「这个
//! 档位值不值得注入上游」。网关那份在 `core::model_rules` 里，与「模型映射 /
//! 等级绑定」的配置机制挤在一起 —— 插件**没有那份配置**（映射由网关持有，
//! 绑定解析也在网关侧完成），但「哪些值是正向档位」这条判据本身是纯知识。
//!
//! ── 为什么值必须与网关逐字一致 ──────────────────────────────
//! 同一条请求在「内置家」与「插件家」上的档位处理不能分叉：网关侧
//! `reasoning_patch` 的调用方按同一张表判「要不要注入」。表变了就两边都变
//! （网关的 `EFFORT_LEVELS` 是事实来源，这里抄一份并注明出处）。

/// 通用表里**可以翻译给上游**的正向档位（由弱到强，不含 `off` / `none`）。
///
/// 与网关 `core::model_rules::reasoning::EFFORT_LEVELS` **逐字相同**。
const EFFORT_LEVELS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];

/// 该等级是不是「关闭思考」两档（`off` / `none`）。
///
/// 与网关 `core::model_rules::is_thinking_off` **逐字相同**。搬来的家（Accio）
/// 用它把「关闭思考」与「表外值」分开措辞 —— 两者都不注入，但用户看到的原因
/// 完全不同，日志里含糊过去就等于没说。
pub fn reasoning_is_off(level: &str) -> bool {
    let trimmed = level.trim();
    trimmed.eq_ignore_ascii_case("off") || trimmed.eq_ignore_ascii_case("none")
}

/// 等级字符串的长度上限（与网关 `REASONING_MAX_CHARS` 同值）。
const REASONING_MAX_CHARS: usize = 32;

/// 从**下游请求体**里读客户端显式指定的思考等级。
///
/// ── 为什么插件里要有一份 ────────────────────────────────────
/// 搬来的家（Accio）在 `reasoning_patch` 里要判「客户端是不是已经指定过档位」
/// —— 指定过就让位，不覆盖用户的明确意图。网关那版读的是
/// `core::model_rules::read_client_level`，插件没有那份配置层，但「哪些键算
/// 客户端档位」这条判据是纯知识。
///
/// 取值链与网关**逐字相同**：`reasoning_effort` → `reasoningEffort` →
/// `effort` → `reasoning`（对象形态取 `effort` 子键）→ `thinking`。
pub fn read_client_level(body: &serde_json::Value) -> Option<String> {
    for key in ["reasoning_effort", "reasoningEffort", "effort", "reasoning", "thinking"] {
        match body.get(key) {
            Some(serde_json::Value::String(text)) => return normalize(text),
            Some(serde_json::Value::Object(object)) if key == "reasoning" => {
                if let Some(serde_json::Value::String(effort)) = object.get("effort") {
                    if let Some(level) = normalize(effort) {
                        return Some(level);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// 归一化一个思考等级：去首尾空白 → 过长或空串返回 `None`（= 不覆盖）。
///
/// 与网关 `core::model_rules::normalize` 同口径：**不校验是否在档位表里**
/// （界面提供「自定义等级」入口），只挡空串与超长值。
fn normalize(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.chars().count() > REASONING_MAX_CHARS {
        return None;
    }
    Some(trimmed.to_string())
}

/// 一个档位在强弱序里的下标（`None` = 不在表内，不该注入上游）。
pub fn reasoning_rank(level: &str) -> Option<usize> {
    let trimmed = level.trim();
    if trimmed.is_empty() {
        return None;
    }
    EFFORT_LEVELS
        .iter()
        .position(|known| trimmed.eq_ignore_ascii_case(known))
}
