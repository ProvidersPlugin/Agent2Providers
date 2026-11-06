//! 档位字段来自 1.1.5 Box-Agent 的 `_apply_thinking_params`；不覆盖客户端选择。

use agent2api_provider_protocol as proto;
use serde_json::{json, Value};

/// 上游（`/api/web/llm/v2/chat/completions`）接受的思考档位值。
///
/// 实测（2026-11-04，直连上游）：`minimal` / `low` / `medium` / `high` /
/// `xhigh` / `max` / `ultra` 均返回 200，且思考量随档位变化
/// （同一提示词：low → reasoning 89 tokens，max → 142）；
/// `off` 与表外值返回 400（`invalid_request_error`）。
///
/// 上游只认 `reasoning_effort` 这个键名 —— 传 `effort` 直接 400
/// （`Unexpected keyword argument 'effort'`）。
const SUPPORTED_EFFORTS: &[&str] = &[
    "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

fn is_supported_effort(value: &str) -> bool {
    SUPPORTED_EFFORTS
        .iter()
        .any(|known| value.eq_ignore_ascii_case(known))
}

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
    // 表外档位上游会 400（`invalid_request_error`），别把它送上去。
    if !is_supported_effort(level) {
        return skip("该思考等级上游不接受，不注入");
    }
    let (field, value) = match family(model) {
        // 「thinking」家族上游认 `reasoning_effort`，注入**档位名**而不是只给
        // 一个「开思考」的开关 —— 否则映射上配的档位到了上游就丢了。
        "thinking" => ("reasoning_effort", json!(level)),
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
            let effort = fields
                .get("reasoning_effort")
                .and_then(Value::as_str)
                .map(str::to_string);
            match effort {
                Some(effort) if crate::reasoning::is_thinking_off(&effort) => {
                    fields.remove("reasoning_effort");
                    fields.insert("thinking".to_string(), json!({ "type": "disabled" }));
                }
                Some(effort) if is_supported_effort(&effort) => {
                    // 原样保留：档位名要一路带到上游，也带到网关的日志里。
                    fields.insert("reasoning_effort".to_string(), json!(effort));
                }
                Some(_) => {
                    // 表外值上游会 400（`invalid_request_error`）—— 删掉它，
                    // 退回「开思考」的默认形态，至少请求不会整个失败。
                    fields.remove("reasoning_effort");
                    fields.insert("thinking".to_string(), json!({ "type": "enabled" }));
                }
                None => {}
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
    // 优先读 `reasoning_effort` —— 那是真正随请求发出的档位（上游认这个键名，
    // `normalize()` 会保留它）。
    //
    // 只有当出站体里确实**没有任何**档位信息时，才从 `thinking.type` 反推：
    // 「开了思考但没有具体档位」是一个真实存在的状态，报成 "high" 会让网关的
    // 请求日志显示一个上游并未收到的档位 —— 用户看到 (high) 会以为上游收到了
    // high，实际上它只收到了「开思考」。
    //
    // 这里返回 `None`（= 没有等级随行），与网关侧 `upstream_reasoning` 文档里
    // 「空串 = 没有等级随行」的定义一致。
    body.get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(
            || match body.pointer("/thinking/type").and_then(Value::as_str) {
                Some("disabled") => Some("off".to_string()),
                _ => None,
            },
        )
}
