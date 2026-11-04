//! 档位字段来自 1.1.5 Box-Agent 的 `_apply_thinking_params`；不覆盖客户端选择。

use agent2api_provider_protocol as proto;
use serde_json::{json, Value};

fn model_name(model: &str) -> String {
    model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .trim()
        .to_ascii_lowercase()
}

fn family(model: &str) -> &'static str {
    let model = model_name(model);
    if model.starts_with("sn-deepseek-") || model.starts_with("deepseek-") {
        "thinking"
    } else if model.starts_with("sn-glm-5-3") || model.starts_with("sn-kimi-k3") {
        "low_high"
    } else if model.starts_with("sn-sensenova-") || model.starts_with("raccoon-") {
        "effort"
    } else {
        ""
    }
}

pub(super) fn supported(model: &str) -> bool {
    !family(model).is_empty()
}

fn explicit(body: &Value) -> bool {
    [
        "reasoning_effort",
        "reasoningEffort",
        "reasoning",
        "thinking",
        "enable_thinking",
    ]
    .iter()
    .any(|key| body.get(*key).is_some_and(|value| !value.is_null()))
}

pub(super) fn patch(level: &str, model: &str, body: &Value) -> proto::ReasoningPatch {
    let skip = |reason: &str| proto::ReasoningPatch::Skip {
        reason: reason.to_string(),
    };
    if explicit(body) {
        return skip("客户端已指定思考参数，保留原值");
    }
    let Some(rank) = crate::reasoning::effort_rank(level) else {
        return skip("关闭思考或未知档位不注入绑定");
    };
    let (field, value) = match family(model) {
        "thinking" => ("thinking", json!({ "type": "enabled" })),
        "low_high" => (
            "reasoning_effort",
            json!(if rank <= 1 { "low" } else { "high" }),
        ),
        "effort" => (
            "reasoning_effort",
            json!(match rank {
                0 | 1 => "low",
                2 => "medium",
                _ => "high",
            }),
        ),
        _ => return skip("该小浣熊模型没有已确认的思考参数"),
    };
    proto::ReasoningPatch::Set {
        field: field.to_string(),
        value,
    }
}

pub(super) fn normalize(body: &Value, model: &str) -> Value {
    let mut body = body.clone();
    let Some(fields) = body.as_object_mut() else {
        return body;
    };
    if !fields.contains_key("reasoning_effort") {
        if let Some(effort) = fields.remove("reasoningEffort") {
            fields.insert("reasoning_effort".to_string(), effort);
        }
    }
    match family(model) {
        // 正向档位**原样留在 `reasoning_effort`**：官方客户端只发开关，但上游的
        // `/chat/completions` 接受命名档位（3.0.8 起就是原样透传，实测 max 能到
        // 上游并 200）。删掉它等于把客户端的档位选择抹平成「开思考」—— 用户看到
        // 的 upstream=high 只是 `outbound` 对裸开关的展示口径，并非上游真收到了
        // high。只有「关闭思考」两档才改写成 `thinking.disabled`：`none` / `off`
        // 不是所有上游都认，而开关能明确表达关闭。
        "thinking" if !fields.get("thinking").is_some_and(|value| !value.is_null()) => {
            let off = fields
                .get("reasoning_effort")
                .and_then(Value::as_str)
                .is_some_and(crate::reasoning::is_thinking_off);
            if off {
                fields.remove("reasoning_effort");
                fields.insert("thinking".to_string(), json!({ "type": "disabled" }));
            }
        }
        "low_high" => {
            let disabled = fields
                .get("thinking")
                .and_then(|value| value.get("type"))
                .and_then(Value::as_str)
                == Some("disabled");
            if disabled {
                fields.remove("thinking");
                fields.insert("reasoning_effort".to_string(), json!("low"));
            } else if let Some(effort) = fields.get("reasoning_effort").and_then(Value::as_str) {
                let mapped = if crate::reasoning::is_thinking_off(effort)
                    || crate::reasoning::effort_rank(effort).is_some_and(|rank| rank <= 1)
                {
                    Some("low")
                } else if crate::reasoning::effort_rank(effort).is_some() {
                    Some("high")
                } else {
                    None
                };
                if let Some(mapped) = mapped {
                    fields.insert("reasoning_effort".to_string(), json!(mapped));
                }
            }
        }
        _ => {}
    }
    body
}

pub(super) fn outbound(body: &Value) -> Option<String> {
    body.get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(
            || match body.pointer("/thinking/type").and_then(Value::as_str) {
                Some("enabled") => Some("high".to_string()),
                Some("disabled") => Some("off".to_string()),
                _ => None,
            },
        )
}
