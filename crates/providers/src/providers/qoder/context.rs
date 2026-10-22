//! Qoder 上下文窗口档位（200K / 400K / 1M）：目录解析、按 prompt 选档、落到请求体。
//!
//! ── 为什么需要它 ────────────────────────────────────────────
//! 上游每个模型条目带 `context_config`（多档上下文窗口）与 `max_input_tokens`
//! （**上游当前选中的那一档**，实测 qfmodel 是 180000，而它最大支持 1M）。
//! Qoder IDE 让用户在模型选择器里切档；我们这种 CLI 形态的客户端没有那个选择器
//! —— 长会话（Claude Code / Codex 一路攒上下文）一旦超过默认档就会被上游拒绝，
//! 尽管模型本身支持更大。本模块模拟 IDE 的选择：估算 prompt 大小、挑**最小的
//! 够用档位**（绝不低于上游当前档），再写进 IDE 写的那三个位置。
//!
//! ── 落到哪三处（与参考实现 `applyQoderContextTier` 逐字对应）──
//!   `parameters.context_length`
//!   `chat_context.extra.ideModelConfigOverride.max_input_tokens`
//!   `model_config.max_input_tokens`
//! 三者是 IDE 同时写的位置，缺一个就可能出现「服务端按老档裁上下文」的情形。
//!
//! ── 估算为什么是「CJK 一字一 token」而不是简单的 chars/4 ──────
//! chars/4 对中文会低估到四分之一 —— 而中文长会话正是「会不会超档」这件事最
//! 常见的情形。参考实现同款口径（CJK 记 1 token，其余 4 字符记 1 token）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic。

use serde_json::{json, Value};

/// 估算值上浮比例（分词器差异与工具描述的固定开销；参考实现同款 15%）
const HEADROOM: f64 = 0.15;

/// 一个档位
#[derive(Clone, Debug)]
pub struct Tier {
    /// 档位名（`200K` / `400K` / `1M`）
    pub name: String,
    /// 该档的输入上限（token）
    pub tokens: i64,
    /// 本次 prompt 的估算值（token，日志用；只算一次，见 `describe`）
    pub estimated: i64,
    /// 升档原因（日志用：`auto:fits` / `auto:largest`）
    pub reason: &'static str,
}

/// 目录条目 → 归一后的档位表 `[{name, tokens, isDefault}]`（按 tokens 升序去重）。
///
/// ── 为什么两种形态都要收 ────────────────────────────────────
/// 上游给的是**对象映射**（键就是档位名）：
/// `{"200K":{"token_count":200000,"is_default":true},"1M":{"token_count":1000000}}`；
/// 参考实现按**数组**解析（`[{name, token_count, is_default}]`），两种形态在一手
/// 目录里都出现过。归一放在这里，选档逻辑就只见一种形状。
pub fn tiers_from_catalog(item: &Value) -> Vec<Value> {
    let Some(config) = item.get("context_config") else {
        return Vec::new();
    };
    let mut tiers: Vec<(i64, String, bool)> = Vec::new();
    let mut push = |fallback_name: &str, entry: &Value| {
        let tokens = entry
            .get("token_count")
            .or_else(|| entry.get("tokenCount"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if tokens <= 0 {
            return;
        }
        // 名字优先取条目自己的 name / key（数组形态），其次用映射的键（对象形态）
        let name = ["name", "label", "key", "id"]
            .iter()
            .find_map(|key| entry.get(*key).and_then(Value::as_str))
            .filter(|text| !text.trim().is_empty())
            .unwrap_or(fallback_name)
            .trim()
            .to_string();
        let is_default = entry
            .get("is_default")
            .or_else(|| entry.get("isDefault"))
            .map(super::protocol::truthy)
            .unwrap_or(false);
        tiers.push((tokens, name, is_default));
    };
    match config {
        Value::Object(map) => {
            for (key, entry) in map {
                if entry.is_object() {
                    push(key, entry);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                if item.is_object() {
                    push("", item);
                }
            }
        }
        _ => {}
    }
    tiers.sort_by_key(|(tokens, _, _)| *tokens);
    tiers.dedup_by_key(|(tokens, _, _)| *tokens);
    tiers
        .into_iter()
        .map(|(tokens, name, is_default)| {
            json!({
                "name": if name.is_empty() { tokens.to_string() } else { name },
                "tokens": tokens,
                "isDefault": is_default,
            })
        })
        .collect()
}

/// 档位表里最大的那一档（没有档位时给 0，调用方按缺省窗口兜底）
pub fn max_tokens(tiers: &[Value]) -> i64 {
    tiers
        .iter()
        .filter_map(|tier| tier.get("tokens").and_then(Value::as_i64))
        .max()
        .unwrap_or(0)
}

/// prompt 大小的粗略估算（token）：CJK 字符约 1 token，其余约 4 字符 1 token。
///
/// 估的是**将要发出去的那份内容**（消息 + 工具定义），不含信封里的固定字段 ——
/// 那些字段的量级相对可忽略，计入反而会让每次请求都多留一份余量。
pub fn estimate_prompt_tokens(messages: &[Value], tools: Option<&Vec<Value>>) -> i64 {
    let payload = json!({
        "messages": messages,
        "tools": tools.cloned().unwrap_or_default(),
    });
    let text = payload.to_string();
    let mut cjk = 0i64;
    let mut total = 0i64;
    for ch in text.chars() {
        total += 1;
        if is_cjk(ch) {
            cjk += 1;
        }
    }
    let rest = total - cjk;
    // 向上取整：宁可多留一点余量，也不要在边界上被上游拒
    cjk + (rest + 3) / 4
}

/// 这个字符算不算「一字一 token」的方块字（与参考实现同一组码位区间：
/// 谚文、CJK 部首扩展与统一表意文字、兼容表意文字、全角形式）
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x11ff
        | 0x2e80..=0x9fff
        | 0xac00..=0xd7af
        | 0xf900..=0xfaff
        | 0xff00..=0xffef)
}

/// 选档：返回 `None` = 保持上游默认档（不写任何字段，与改造前完全一致）。
///
/// ── 判据链（与参考实现 `resolveQoderContextTier` 的 auto 模式一致）──
///   1. 模型没有档位表 → 不升级（离线兜底清单就没有档位）；
///   2. 估算值 ×(1+余量) 不超过**上游当前档**（`max_input_tokens`，缺省用
///      `is_default` 的那档）→ 不升级：多数请求都走这条，行为零变化；
///   3. 超了就挑「最小的、比当前档大、且装得下」的那档；都装不下就用最大档。
pub fn resolve(
    model_config: &Value,
    messages: &[Value],
    tools: Option<&Vec<Value>>,
) -> Option<Tier> {
    let tiers: Vec<(String, i64, bool)> = model_config
        .get("tiers")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|tier| {
                    let tokens = tier.get("tokens").and_then(Value::as_i64)?;
                    if tokens <= 0 {
                        return None;
                    }
                    Some((
                        tier.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                        tokens,
                        tier.get("isDefault").map(super::protocol::truthy).unwrap_or(false),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    if tiers.is_empty() {
        return None;
    }

    let default_tier = tiers
        .iter()
        .find(|(_, _, is_default)| *is_default)
        .or_else(|| tiers.first())?;
    let current = model_config
        .get("max_input_tokens")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .unwrap_or(default_tier.1);

    let estimated = estimate_prompt_tokens(messages, tools);
    let need = (estimated as f64 * (1.0 + HEADROOM)).ceil() as i64;
    if need <= current {
        return None;
    }

    let fits = tiers
        .iter()
        .find(|(_, tokens, _)| *tokens >= need && *tokens > current);
    let (name, tokens, reason) = match fits {
        Some((name, tokens, _)) => (name.clone(), *tokens, "auto:fits"),
        None => {
            let (name, tokens, _) = tiers.iter().max_by_key(|(_, tokens, _)| *tokens)?;
            if *tokens <= current {
                return None;
            }
            (name.clone(), *tokens, "auto:largest")
        }
    };
    Some(Tier { name, tokens, estimated, reason })
}

/// 把选中的档位写进请求体（三个位置，见模块头）。
pub fn apply(payload: &mut Value, tier: &Tier) {
    if tier.tokens <= 0 {
        return;
    }
    if let Some(parameters) = payload.get_mut("parameters").and_then(Value::as_object_mut) {
        parameters.insert("context_length".to_string(), Value::from(tier.tokens));
    }
    if let Some(extra) = payload
        .pointer_mut("/chat_context/extra")
        .and_then(Value::as_object_mut)
    {
        extra.insert(
            "ideModelConfigOverride".to_string(),
            json!({ "max_input_tokens": tier.tokens }),
        );
    }
    if let Some(config) = payload.get_mut("model_config").and_then(Value::as_object_mut) {
        config.insert("max_input_tokens".to_string(), Value::from(tier.tokens));
    }
}

/// 给日志用的一行摘要（如 `400K（估计 268k token，auto:fits）`）
pub fn describe(tier: &Tier) -> String {
    format!("{}（估计 {}k token，{}）", tier.name, tier.estimated / 1000, tier.reason)
}
