//! 原生Cline白名单请求体。保留工具循环/多模态消息，通道前缀不可剥除。

use super::{account, models};
use agent2api_wasm_sdk::{json, Error, Result, Value};

const PASS: &[&str] = &[
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "functions",
    "function_call",
    "temperature",
    "top_p",
    "top_k",
    "stop",
    "presence_penalty",
    "frequency_penalty",
    "response_format",
    "user",
    "n",
    "logit_bias",
    "seed",
    "logprobs",
    "top_logprobs",
    "stream_options",
    "metadata",
];

pub fn build(mut input: Value, session: &str) -> Result<Value> {
    let model = input["model"]
        .as_str()
        .filter(|model| !model.is_empty())
        .ok_or_else(|| Error::http(400, "Cline 请求缺少模型名称"))?;
    let model = models::wire_id(model)?;
    let max_tokens = account::number(&input["max_tokens"])
        .or_else(|| account::number(&input["max_completion_tokens"]))
        .filter(|value| *value >= 1.0 && *value < i64::MAX as f64)
        .map(|value| value as i64)
        .unwrap_or(128_000);
    let reasoning = ["reasoning_effort", "reasoningEffort"]
        .iter()
        .find_map(|key| {
            input[*key]
                .as_str()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or("high")
        .to_string();
    let object = input
        .as_object_mut()
        .ok_or_else(|| Error::http(400, "Cline 请求体必须是 JSON 对象"))?;
    let mut output = json!({ "model": model, "max_tokens": max_tokens, "session_id": session,
        "reasoning_effort": reasoning, "stream": true });
    if let Some(messages) = object.remove("messages") {
        output["messages"] = messages;
    }
    for key in PASS {
        if let Some(value) = object.remove(*key) {
            output[*key] = value;
        }
    }
    Ok(output)
}
