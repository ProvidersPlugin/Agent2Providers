//! CodeArts 只接受不超过64字符的工具声明；缩名与还原均在本次请求内，不改工具ID或参数。
//! 原生tool_names.rs及公开magpie CodeArts实现均有该限制，使用SHA256后缀避免同前缀碰撞。

use std::collections::{HashMap, HashSet};

use agent2api_wasm_sdk::{Result, Value};
use sha2::{Digest, Sha256};

use crate::{errors, util};

pub struct Restorer(HashMap<String, String>);

pub fn shorten(body: &mut Value) -> Result<Restorer> {
    let mut mapping: HashMap<String, String> = HashMap::new();
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return Ok(Restorer(mapping));
    };
    let mut taken: HashSet<String> = tools
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .map(str::to_string)
        .collect();
    for tool in tools {
        let Some(name) = tool["function"]["name"].as_str().map(str::to_string) else {
            continue;
        };
        if name.chars().count() <= 64 {
            continue;
        }
        if let Some((sent, _)) = mapping.iter().find(|(_, original)| **original == name) {
            tool["function"]["name"] = Value::String(sent.clone());
            continue;
        }
        let digest = util::hex(&Sha256::digest(name.as_bytes()));
        let mut replacement = None;
        for width in [8, 12, 16, 32] {
            let prefix: String = name
                .chars()
                .take(63 - width)
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-') {
                        ch
                    } else {
                        '_'
                    }
                })
                .collect();
            let sent = format!("{prefix}_{}", &digest[..width]);
            if taken.insert(sent.clone()) {
                replacement = Some(sent);
                break;
            }
        }
        let sent = replacement
            .ok_or_else(|| errors::invalid("CodeArts 工具名称缩写冲突，未把不同工具合并"))?;
        tool["function"]["name"] = Value::String(sent.clone());
        mapping.insert(sent, name);
    }
    Ok(Restorer(mapping))
}

impl Restorer {
    pub fn apply(&self, event: &mut Value) {
        if self.0.is_empty() {
            return;
        }
        let Some(choices) = event.get_mut("choices").and_then(Value::as_array_mut) else {
            return;
        };
        for choice in choices {
            for slot in ["delta", "message"] {
                let Some(calls) = choice
                    .get_mut(slot)
                    .and_then(|part| part.get_mut("tool_calls"))
                    .and_then(Value::as_array_mut)
                else {
                    continue;
                };
                for call in calls {
                    let Some(function) = call.get_mut("function").and_then(Value::as_object_mut)
                    else {
                        continue;
                    };
                    let original = function
                        .get("name")
                        .and_then(Value::as_str)
                        .and_then(|sent| self.0.get(sent))
                        .cloned();
                    if let Some(original) = original {
                        function.insert("name".into(), Value::String(original));
                    }
                }
            }
        }
    }
}
