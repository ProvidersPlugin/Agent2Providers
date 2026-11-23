//! WorkBuddy 的档位来自当前地区的模型目录，不能由模型家族名推断。
//! `reasoning.effort` 且没有可选列表表示固定档；未声明档位时不注入绑定。

use serde_json::{json, Map, Value};

fn normalize(level: &str) -> Option<String> {
    let level = level.trim();
    (!level.is_empty() && level.chars().count() <= 32).then(|| level.to_string())
}
fn is_thinking_off(level: &str) -> bool {
    matches!(level.trim().to_ascii_lowercase().as_str(), "off" | "none")
}
fn effort_rank(level: &str) -> Option<usize> {
    ["minimal", "low", "medium", "high", "xhigh", "max"]
        .iter()
        .position(|s| level.trim().eq_ignore_ascii_case(s))
}

pub(super) const EFFORT_FIELD: &str = "reasoning_effort";
const EFFORT_KEYS: [&str; 2] = [EFFORT_FIELD, "reasoningEffort"];

#[derive(Default)]
struct Metadata {
    levels: Vec<String>,
    default: Option<String>,
    fixed: Option<String>,
    supports_reasoning: Option<bool>,
}

impl Metadata {
    fn from_model(model: Option<&Value>) -> Self {
        let Some(model) = model else {
            return Self::default();
        };
        let levels = model
            .pointer("/reasoning/supportedEfforts")
            .and_then(Value::as_array)
            .map(|items| {
                let mut levels = Vec::<String>::new();
                for level in items.iter().filter_map(Value::as_str).filter_map(normalize) {
                    if !levels
                        .iter()
                        .any(|known| known.eq_ignore_ascii_case(&level))
                    {
                        levels.push(level);
                    }
                }
                levels
            })
            .unwrap_or_default();
        let declared = model
            .pointer("/reasoning/effort")
            .and_then(Value::as_str)
            .and_then(normalize);
        let fixed = if levels.is_empty() {
            declared.clone()
        } else if levels.len() == 1 {
            levels.first().cloned()
        } else {
            None
        };
        let default = model
            .pointer("/reasoning/defaultEffort")
            .and_then(Value::as_str)
            .and_then(normalize)
            .or(declared)
            .and_then(|value| {
                if levels.is_empty() {
                    Some(value)
                } else {
                    levels
                        .iter()
                        .find(|level| level.eq_ignore_ascii_case(&value))
                        .cloned()
                }
            })
            .or_else(|| fixed.clone());
        Self {
            levels,
            default,
            fixed,
            supports_reasoning: model.get("supportsReasoning").and_then(Value::as_bool),
        }
    }

    fn bindable(&self) -> bool {
        self.supports_reasoning != Some(false)
            && (self.levels.iter().any(|level| !is_thinking_off(level))
                || self
                    .fixed
                    .as_deref()
                    .is_some_and(|level| !is_thinking_off(level)))
    }

    fn resolve(&self, requested: &str) -> Option<String> {
        if !self.bindable() {
            return None;
        }
        let requested = requested.trim();
        if let Some(level) = self
            .levels
            .iter()
            .find(|level| level.eq_ignore_ascii_case(requested))
        {
            return Some(level.clone());
        }
        if self
            .fixed
            .as_deref()
            .is_some_and(|level| level.eq_ignore_ascii_case(requested))
        {
            return self.fixed.clone();
        }
        let rank = effort_rank(requested)?;
        if let Some(fixed) = &self.fixed {
            return Some(fixed.clone());
        }
        let ranked: Vec<_> = self
            .levels
            .iter()
            .filter_map(|level| effort_rank(level).map(|rank| (rank, level)))
            .collect();
        ranked
            .iter()
            .filter(|(candidate, _)| *candidate <= rank)
            .max_by_key(|(candidate, _)| *candidate)
            .or_else(|| ranked.iter().min_by_key(|(candidate, _)| *candidate))
            .map(|(_, level)| (*level).clone())
            .or_else(|| self.default.clone())
    }
}

fn thinking_disabled(body: &Value) -> bool {
    body.pointer("/thinking/type")
        .and_then(Value::as_str)
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("disabled"))
}

fn has_client_choice(body: &Value) -> bool {
    EFFORT_KEYS
        .iter()
        .any(|key| body.get(*key).is_some_and(|value| !value.is_null()))
        || body
            .pointer("/reasoning/effort")
            .is_some_and(|value| !value.is_null())
        || body.get("thinking").is_some_and(|value| match value {
            Value::Null => false,
            // 仅开启思考不是选择档位；关闭、预算或未知格式仍优先保留客户端意图。
            Value::Object(fields) => {
                fields
                    .get("budget_tokens")
                    .is_some_and(|value| !value.is_null())
                    || fields.get("type").is_some_and(|value| {
                        !value.is_null()
                            && !value.as_str().is_some_and(|text| {
                                text.trim().is_empty()
                                    || text.trim().eq_ignore_ascii_case("enabled")
                            })
                    })
            }
            _ => true,
        })
}

fn client_effort(body: &Value) -> Option<&str> {
    EFFORT_KEYS
        .iter()
        .find_map(|key| body.get(*key).and_then(Value::as_str))
        .or_else(|| body.pointer("/reasoning/effort").and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub(super) fn patch(level: &str, model: Option<&Value>, body: &Value) -> Value {
    let skip = |reason: &str| json!({ "action": "skip", "reason": reason });
    if is_thinking_off(level) {
        return skip("映射绑定不注入关闭思考字段");
    }
    if has_client_choice(body) {
        return skip("客户端已显式指定思考参数，映射绑定不覆盖");
    }
    let metadata = Metadata::from_model(model);
    if !metadata.bindable() {
        return skip("当前通道未声明可用档位，跳过绑定；请先获取模型目录");
    }
    match metadata.resolve(level) {
        Some(effective) => json!({ "action": "set", "field": EFFORT_FIELD, "value": effective }),
        None => skip("绑定等级不在上游声明的档位中，且无法安全归一"),
    }
}

pub(super) fn annotate(model: &mut Value) {
    let metadata = Metadata::from_model(Some(model));
    let Some(object) = model.as_object_mut() else {
        return;
    };
    let known =
        !metadata.levels.is_empty() || metadata.fixed.is_some() || metadata.default.is_some();
    object.insert(
        "reasoningBindingSupported".into(),
        Value::Bool(metadata.bindable()),
    );
    object.insert(
        "reasoningSource".into(),
        json!(if known { "upstream" } else { "undeclared" }),
    );
    let levels = if metadata.supports_reasoning == Some(false) {
        Vec::new()
    } else if metadata.levels.is_empty() {
        metadata.fixed.clone().into_iter().collect()
    } else {
        metadata.levels.clone()
    };
    object.insert("reasoningLevels".into(), json!(levels));
    object.remove("reasoningDefaultLevel");
    object.remove("reasoningFixedLevel");
    if metadata.supports_reasoning != Some(false) {
        if let Some(default) = metadata.default {
            object.insert("reasoningDefaultLevel".into(), json!(default));
        }
        if let Some(fixed) = metadata.fixed {
            object.insert("reasoningFixedLevel".into(), json!(fixed));
        }
    }
}

pub(super) fn normalize_outbound(body: &mut Value, model: Option<&Value>) {
    let metadata = Metadata::from_model(model);
    if thinking_disabled(body) {
        if let Some(object) = body.as_object_mut() {
            for key in EFFORT_KEYS {
                object.remove(key);
            }
            if let Some(reasoning) = object.get_mut("reasoning").and_then(Value::as_object_mut) {
                reasoning.remove("effort");
            }
            object.remove("reasoning_summary");
            backfill_traced(object);
        }
        return;
    }
    let small_budget = body["max_tokens"]
        .as_u64()
        .is_some_and(|limit| limit > 0 && limit <= 1024);
    let given = client_effort(body).map(str::to_string);
    let explicit = has_client_choice(body);
    let Some(object) = body.as_object_mut() else {
        return;
    };
    if let Some(requested) = given.as_deref() {
        if !is_thinking_off(requested) {
            if let Some(effective) = metadata.resolve(requested) {
                write_effort(object, &effective);
            } else if !object.contains_key(EFFORT_FIELD) {
                write_effort(object, requested);
            }
        } else {
            write_effort(object, requested);
        }
    } else if (!explicit
        || object
            .get("thinking")
            .and_then(|value| value.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|value| value.eq_ignore_ascii_case("enabled")))
        && !EFFORT_KEYS.iter().any(|key| object.contains_key(*key))
    {
        // 只补目录默认值；短输出预算不默认耗在思考上，客户端显式档位仍保留。
        if let Some(default) = metadata
            .default
            .as_deref()
            .filter(|_| metadata.supports_reasoning != Some(false) && !explicit && !small_budget)
        {
            write_effort(object, default);
        }
    }
    let effort = object
        .get(EFFORT_FIELD)
        .and_then(Value::as_str)
        .map(str::to_string);
    let enabled = effort
        .as_deref()
        .is_some_and(|value| !is_thinking_off(value));
    let deepseek = object
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("deepseek"));
    if deepseek && enabled {
        match object.get_mut("thinking") {
            Some(Value::Object(fields))
                if fields.get("type").is_none_or(|value| {
                    value.is_null() || value.as_str().is_some_and(|value| value.trim().is_empty())
                }) =>
            {
                fields.insert("type".into(), json!("enabled"));
            }
            None | Some(Value::Null) => {
                object.insert("thinking".into(), json!({ "type": "enabled" }));
            }
            _ => {}
        }
    }
    if enabled && !object.contains_key("reasoning_summary") {
        object.insert("reasoning_summary".into(), json!("auto"));
    }
    if deepseek
        && (enabled
            || object
                .get("thinking")
                .and_then(|thinking| thinking.get("type"))
                .and_then(Value::as_str)
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("enabled")))
    {
        backfill_reasoning(object);
    } else {
        backfill_traced(object);
    }
}

fn backfill_traced(object: &mut Map<String, Value>) {
    let deepseek = object
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("deepseek"));
    let traced = object
        .get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages
                .iter()
                .filter(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
                .any(|message| {
                    ["reasoning_content", "reasoning"].iter().any(|key| {
                        message
                            .get(*key)
                            .and_then(Value::as_str)
                            .is_some_and(|trace| !trace.is_empty())
                    })
                })
        });
    if deepseek && traced {
        backfill_reasoning(object);
    }
}

fn write_effort(object: &mut Map<String, Value>, level: &str) {
    object.insert(EFFORT_FIELD.into(), json!(level));
    object.remove("reasoningEffort");
    if let Some(reasoning) = object.get_mut("reasoning").and_then(Value::as_object_mut) {
        reasoning.remove("effort");
    }
}

fn backfill_reasoning(object: &mut Map<String, Value>) {
    let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let trace = message
            .get("reasoning_content")
            .and_then(Value::as_str)
            .or_else(|| message.get("reasoning").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        let Some(fields) = message.as_object_mut() else {
            continue;
        };
        if !fields
            .get("reasoning_content")
            .is_some_and(Value::is_string)
        {
            fields.insert("reasoning_content".into(), json!(trace));
        }
        // 上游校验 reasoning 长度；第三方客户端丢失思考文本时只补空白占位。
        if !fields
            .get("reasoning")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
        {
            fields.insert(
                "reasoning".into(),
                json!(if trace.is_empty() { " " } else { &trace }),
            );
        }
    }
}

pub(super) fn outbound_level(body: &Value, model: Option<&Value>) -> Option<String> {
    if thinking_disabled(body) {
        return None;
    }
    let given = client_effort(body);
    if given.is_some_and(is_thinking_off) {
        return None;
    }
    let metadata = Metadata::from_model(model);
    if metadata.supports_reasoning == Some(false) {
        return None;
    }
    metadata
        .fixed
        .or_else(|| given.map(str::to_string))
        .or(metadata.default)
        .filter(|value| !is_thinking_off(value))
}
