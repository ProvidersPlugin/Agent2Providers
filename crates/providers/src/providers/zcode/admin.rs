//! 无账号依赖的机器级管理动作；国内版与国际版共用本进程的令牌池。

use serde_json::{json, Map, Value};

use super::{captcha, plan, OFFICIAL_PROMPT_NOTE};

pub(super) fn machine_action(action: &str, payload: &Value) -> Option<Result<Value, String>> {
    let outcome = match action {
        "captcha.stats" => captcha::stats(),
        "captcha.push" => {
            let accepted = payload.get("tokens").and_then(Value::as_array)
                .map(|items| items.iter().filter(|item| {
                    let param = item.get("param").and_then(Value::as_str).map(str::trim).unwrap_or("");
                    if param.is_empty() { return false }
                    let region = item.get("region").and_then(Value::as_str).unwrap_or("");
                    captcha::push(param, region);
                    true
                }).count()).unwrap_or(0);
            json!({ "accepted": accepted })
        }
        "prompt.info" => json!({
            "gatewayNote": OFFICIAL_PROMPT_NOTE,
            "gatewayChars": plan::official_prompt_approx_chars(),
            "gatewayBlocks": plan::official_blocks_template().map(|blocks| {
                let mut map = Map::new();
                for field in plan::GatewayBlocks::FIELDS {
                    map.insert(field.to_string(), Value::String(blocks.get(field).to_string()));
                }
                Value::Object(map)
            }),
        }),
        _ => return None,
    };
    Some(Ok(outcome))
}
