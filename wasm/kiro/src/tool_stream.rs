//! Kiro 工具事件按 toolUseId 跟踪，重复名字不等于新调用，交错分片不串参。
//! 字符串参数是增量，对象参数是完整值；合并后必须是JSON对象，不能悄悄改成{}。
//! 先暂存每个调用，得到stop标记或自然终流时验证后交付，避免错误片段被执行。

use crate::util;
use agent2api_wasm_sdk::{json, Error, Result, Value};

const MAX_CALLS: usize = 128;
const MAX_ARGS: usize = 1024 * 1024;
const MAX_ALL: usize = 4 * 1024 * 1024;
struct Call {
    id: String,
    name: String,
    args: String,
    closed: bool,
    object: bool,
    empty_input: bool,
}
#[derive(Default)]
pub struct Calls {
    calls: Vec<Call>,
    current: Option<usize>,
    bytes: usize,
}
fn bad(message: &str) -> Error {
    Error::http(502, message).with_code("kiro_tool_invalid")
}
fn valid(args: &str) -> Result<()> {
    if !serde_json::from_str::<Value>(args).is_ok_and(|value| value.is_object()) {
        return Err(bad("Kiro 工具参数不是完整 JSON 对象，未交付工具调用"));
    }
    Ok(())
}
impl Calls {
    pub fn push(&mut self, value: &Value) -> Result<()> {
        let id = match value.get("toolUseId") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_str()
                    .filter(|id| util::identifier(id, 256))
                    .ok_or_else(|| bad("Kiro 工具调用编号无效"))?,
            ),
        };
        let name = match value.get("name") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_str()
                    .filter(|name| util::identifier(name, 256))
                    .ok_or_else(|| bad("Kiro 工具名称无效"))?,
            ),
        };
        let found = id.and_then(|id| self.calls.iter().position(|call| call.id == id));
        let index = if let Some(index) = found {
            index
        } else if let Some(id) = id {
            let name = name.ok_or_else(|| bad("Kiro 新工具调用缺少名称"))?;
            if self.calls.len() >= MAX_CALLS {
                return Err(bad("Kiro 工具调用数量超过上限"));
            }
            self.calls.push(Call {
                id: id.into(),
                name: name.into(),
                args: String::new(),
                closed: false,
                object: false,
                empty_input: false,
            });
            self.calls.len() - 1
        } else {
            if self.calls.iter().filter(|call| !call.closed).count() > 1 {
                return Err(bad("Kiro 交错工具分片缺少调用编号，不能猜测归属"));
            }
            self.current
                .ok_or_else(|| bad("Kiro 工具参数没有对应调用"))?
        };
        if value
            .get("stop")
            .is_some_and(|stop| !stop.is_boolean() && !stop.is_null())
        {
            return Err(bad("Kiro 工具终止标记无效"));
        }
        self.current = Some(index);
        let call = &mut self.calls[index];
        if name.is_some_and(|name| name != call.name) {
            return Err(bad("Kiro 同一工具调用返回了不同名称"));
        }
        let input = value.get("input").filter(|value| !value.is_null());
        if call.closed {
            if input.is_some() {
                return Err(bad("Kiro 已结束工具调用又返回参数"));
            }
            return Ok(());
        }
        if let Some(input) = input {
            let text = if let Some(text) = input.as_str() {
                text.to_string()
            } else if input.as_object().is_some_and(|object| object.is_empty())
                && call.args.is_empty()
            {
                // 原生跳过开头{}占位；后续字符串仍可能携带实际参数，无参数工具则在结束时保留{}。
                call.empty_input = true;
                String::new()
            } else if input.is_object() {
                if !call.args.is_empty() {
                    let old: Value = serde_json::from_str(&call.args)
                        .map_err(|_| bad("Kiro 混用了工具参数片段和完整对象"))?;
                    if old != *input {
                        return Err(bad("Kiro 完整工具参数与此前片段冲突"));
                    }
                    call.object = true;
                    String::new()
                } else {
                    call.object = true;
                    input.to_string()
                }
            } else {
                return Err(bad("Kiro 工具参数类型无效"));
            };
            if call.object && input.is_string() && !text.is_empty() {
                return Err(bad("Kiro 完整工具对象之后又追加文本"));
            }
            if call.args.len().saturating_add(text.len()) > MAX_ARGS
                || self.bytes.saturating_add(text.len()) > MAX_ALL
            {
                return Err(bad("Kiro 工具参数超过有界缓冲上限"));
            }
            self.bytes += text.len();
            call.args.push_str(&text);
        }
        if value["stop"] == true {
            valid(if call.args.is_empty() && call.empty_input {
                "{}"
            } else {
                &call.args
            })?;
            call.closed = true;
        }
        Ok(())
    }
    pub fn len(&self) -> usize {
        self.calls.len()
    }
    pub fn finish(&self) -> Result<Vec<Value>> {
        self.calls
            .iter()
            .enumerate()
            .map(|(index, call)| {
                let args = if call.args.is_empty() && call.empty_input {
                    "{}"
                } else {
                    &call.args
                };
                valid(args)?;
                Ok(json!({ "index": index, "id": call.id, "type": "function",
                "function": { "name": call.name, "arguments": args } }))
            })
            .collect()
    }
}
