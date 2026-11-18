//! ADK args_json 是片段，核心 Gemini 转换器只接受一次完整 functionCall。
//! 因此只归并本家工具参数，完成后每个调用向核心发送一次，不把每片变成一个新工具。

use crate::util;
use agent2api_wasm_sdk::{json, Error, Result, Value};

const MAX_CALLS: usize = 256;
const MAX_ARGUMENTS: usize = 1024 * 1024;
const MAX_TOTAL: usize = 4 * 1024 * 1024;

struct Call {
    upstream_id: String,
    id: String,
    name: String,
    arguments: String,
    signature: Option<String>,
}

#[derive(Default)]
pub struct Tools {
    calls: Vec<Call>,
    bytes: usize,
}

fn invalid(message: &str) -> Error {
    Error::http(502, message).with_code("accio_tool_protocol")
}

impl Tools {
    pub fn accept(&mut self, call: &Value, signature: Option<&str>) -> Result<()> {
        let id = util::text(call, &["id"]);
        let name = util::text(call, &["name"]);
        if id.len() > 512
            || name.len() > 256
            || id.chars().any(char::is_control)
            || name.chars().any(char::is_control)
        {
            return Err(invalid("Accio 上游工具调用标识无效"));
        }
        if !call.is_object() {
            return Err(invalid("Accio 工具调用不是有效对象"));
        }
        let raw = call
            .get("argsJson")
            .filter(|value| !value.is_null())
            .or_else(|| call.get("args_json").filter(|value| !value.is_null()))
            .or_else(|| call.get("args"));
        let arguments = match raw {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Null) | None => String::new(),
            Some(value) => value.to_string(),
        };
        if arguments.len() > MAX_ARGUMENTS {
            return Err(invalid("Accio 工具参数超过 1 MiB 限制"));
        }
        let complete =
            |value: &str| !value.trim().is_empty() && serde_json::from_str::<Value>(value).is_ok();
        let found = if !id.is_empty() {
            self.calls.iter().position(|call| call.upstream_id == id)
        } else {
            None
        };
        let found = found.or_else(|| {
            self.calls
                .last()
                .filter(|last| {
                    (id.is_empty() || last.upstream_id.is_empty())
                        && (name.is_empty() || last.name.is_empty() || name == last.name)
                        && !(complete(&last.arguments) && complete(&arguments))
                })
                .map(|_| self.calls.len() - 1)
        });
        let index = match found {
            Some(index) => index,
            None => {
                if self.calls.len() >= MAX_CALLS {
                    return Err(invalid("Accio 单轮工具调用数量超过限制"));
                }
                let local_id = if id.is_empty() {
                    format!("call_{}", util::uuid()?.replace('-', ""))
                } else {
                    id.clone()
                };
                self.calls.push(Call {
                    upstream_id: id.clone(),
                    id: local_id,
                    name: String::new(),
                    arguments: String::new(),
                    signature: None,
                });
                self.calls.len() - 1
            }
        };
        let target = &mut self.calls[index];
        if !name.is_empty() {
            if !target.name.is_empty() && target.name != name {
                return Err(invalid("Accio 同一工具调用的名称发生变化"));
            }
            target.name = name;
        }
        if target.upstream_id.is_empty() && !id.is_empty() {
            target.id = id.clone();
            target.upstream_id = id;
        }
        if let Some(signature) = signature.filter(|signature| !signature.is_empty()) {
            if signature.len() > 64 * 1024 {
                return Err(invalid("Accio 工具签名超过限制"));
            }
            if target
                .signature
                .as_deref()
                .is_some_and(|previous| previous != signature)
            {
                return Err(invalid("Accio 工具签名在同一调用内变化"));
            }
            target.signature = Some(signature.into());
        }
        // 同一显式 id 的完整快照重复到达可忽略；不同 id 永远不合并。
        if !target.upstream_id.is_empty()
            && !arguments.is_empty()
            && target.arguments == arguments
            && complete(&arguments)
        {
            return Ok(());
        }
        let cumulative = !target.upstream_id.is_empty()
            && !target.arguments.is_empty()
            && arguments.starts_with(&target.arguments);
        if complete(&target.arguments) && !arguments.trim().is_empty() && !cumulative {
            return Err(invalid(
                "Accio 工具参数在完整 JSON 后仍被追加，未猜测或重复执行调用",
            ));
        }
        let added = if cumulative {
            arguments.len() - target.arguments.len()
        } else {
            arguments.len()
        };
        self.bytes = self.bytes.saturating_add(added);
        if self.bytes > MAX_TOTAL || target.arguments.len().saturating_add(added) > MAX_ARGUMENTS {
            return Err(invalid("Accio 工具参数累计超过限制"));
        }
        if cumulative {
            target.arguments = arguments;
        } else {
            target.arguments.push_str(&arguments);
        }
        Ok(())
    }

    pub fn has_named(&self) -> bool {
        self.calls.iter().any(|call| !call.name.trim().is_empty())
    }

    pub fn finish(&mut self) -> Result<Vec<Value>> {
        let mut parts = Vec::with_capacity(self.calls.len());
        for call in self.calls.drain(..) {
            if call.name.trim().is_empty() {
                return Err(invalid("Accio 工具参数没有对应工具名"));
            }
            let arguments = if call.arguments.trim().is_empty() {
                "{}".to_string()
            } else {
                call.arguments
            };
            let parsed: Value = serde_json::from_str(&arguments)
                .map_err(|_| invalid("Accio 工具参数流未形成完整 JSON"))?;
            if !parsed.is_object() {
                return Err(invalid("Accio 工具参数必须是 JSON 对象"));
            }
            let mut part =
                json!({ "functionCall": { "id": call.id, "name": call.name, "args": arguments } });
            if let Some(signature) = call.signature {
                part["thoughtSignature"] = json!(signature);
            }
            parts.push(part);
        }
        Ok(parts)
    }
}
