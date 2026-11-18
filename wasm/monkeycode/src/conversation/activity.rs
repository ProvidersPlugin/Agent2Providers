//! 远端Agent活动的严格只读消费者；原始input只验证不保留，绝不转为客户端工具执行。
//! 缺ID保持null，不把title/kind当函数名，不把unknown输入合并成参数增量。

use std::io;

use agent2api_wasm_sdk::{json, Error, Result, Value};

const MAX_INPUT_BYTES: usize = 64 * 1024;
const MAX_INPUT_DEPTH: usize = 16;
const MAX_INPUT_NODES: usize = 4096;
const MAX_READ_BYTES: usize = 128 * 1024 + 75;
const MAX_TITLE_BYTES: usize = 4096;
const MAX_ID_BYTES: usize = 512;
const MAX_KIND_BYTES: usize = 128;
const MAX_METADATA_BYTES: usize = 64 * 1024;

pub(super) struct Activity {
    event: &'static str,
    call_id: Option<String>,
    title: Option<String>,
    kind: Option<String>,
    state: Option<String>,
    input_present: bool,
    read_bytes: usize,
    metadata_bytes: usize,
}

impl Activity {
    /// 当前Task先验证原调用内id；这里只解释真实服务read结果，不导入自由工具对象。
    pub(super) fn from_read(value: &Value) -> Result<Self> {
        exact(
            value,
            &["id", "status", "event", "tool", "done", "replay_forbidden"],
        )?;
        if value["status"] != "tool" || value["done"] != false || value["replay_forbidden"] != true
        {
            return Err(invalid());
        }
        let event = match value["event"].as_str() {
            Some("call") => "call",
            Some("update") => "update",
            _ => return Err(invalid()),
        };
        let tool = &value["tool"];
        exact(
            tool,
            &[
                "call_id",
                "title",
                "kind",
                "input_present",
                "input",
                "input_mode",
                "state",
            ],
        )?;
        if tool["input_mode"] != "unknown" {
            return Err(invalid());
        }
        let call_id = optional_text(&tool["call_id"], MAX_ID_BYTES)?;
        if call_id
            .as_deref()
            .is_some_and(|id| id.is_empty() || id.chars().any(char::is_whitespace))
        {
            return Err(invalid());
        }
        let title = optional_text(&tool["title"], MAX_TITLE_BYTES)?;
        let kind = optional_text(&tool["kind"], MAX_KIND_BYTES)?;
        let state = optional_text(&tool["state"], MAX_KIND_BYTES)?;
        if state.as_deref().is_some_and(|state| state.trim() != state) {
            return Err(invalid());
        }
        let input_present = tool["input_present"].as_bool().ok_or_else(invalid)?;
        if !input_present && !tool["input"].is_null() {
            return Err(invalid());
        }
        // 原input已经是Value；借用限深/节点/字节核验，不克隆、不解析字符串内的JSON。
        let mut nodes = MAX_INPUT_NODES;
        shape(&tool["input"], 0, &mut nodes)?;
        encoded_size(&tool["input"], MAX_INPUT_BYTES)?;
        let read_bytes = encoded_size(value, MAX_READ_BYTES)?;
        let mut activity = Self {
            event,
            call_id,
            title,
            kind,
            state,
            input_present,
            read_bytes,
            metadata_bytes: 0,
        };
        activity.metadata_bytes = encoded_size(&activity.projection(), MAX_METADATA_BYTES)?;
        Ok(activity)
    }

    pub(super) fn projection(&self) -> Value {
        // 明确不含input/rawInput/日志、命令，也没有function/arguments或标准tool_calls。
        json!({
            "source": "remote_agent", "event": self.event, "call_id": self.call_id,
            "title": self.title, "kind": self.kind, "state": self.state,
            "input_present": self.input_present, "input_mode": "unknown", "read_only": true,
        })
    }

    pub(super) fn failed(&self) -> bool {
        self.state.as_deref().is_some_and(|state| {
            state.eq_ignore_ascii_case("failed") || state.eq_ignore_ascii_case("error")
        })
    }

    pub(super) fn read_bytes(&self) -> usize {
        self.read_bytes
    }
    pub(super) fn metadata_bytes(&self) -> usize {
        self.metadata_bytes
    }
}

fn optional_text(value: &Value, limit: usize) -> Result<Option<String>> {
    match value {
        Value::Null => Ok(None),
        Value::String(text) if text.len() <= limit && !text.chars().any(char::is_control) => {
            Ok(Some(text.clone()))
        }
        _ => Err(invalid()),
    }
}

fn exact(value: &Value, fields: &[&str]) -> Result<()> {
    if !value.as_object().is_some_and(|object| {
        object.len() == fields.len() && object.keys().all(|key| fields.contains(&key.as_str()))
    }) {
        return Err(invalid());
    }
    Ok(())
}

fn shape(value: &Value, depth: usize, nodes: &mut usize) -> Result<()> {
    if *nodes == 0 {
        return Err(invalid());
    }
    *nodes -= 1;
    match value {
        Value::String(text) if text.len() > MAX_INPUT_BYTES => return Err(invalid()),
        Value::Array(items) => {
            if depth >= MAX_INPUT_DEPTH {
                return Err(invalid());
            }
            for item in items {
                shape(item, depth + 1, nodes)?;
            }
        }
        Value::Object(fields) => {
            if depth >= MAX_INPUT_DEPTH {
                return Err(invalid());
            }
            for (key, value) in fields {
                if key.len() > MAX_INPUT_BYTES {
                    return Err(invalid());
                }
                shape(value, depth + 1, nodes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn encoded_size(value: &Value, limit: usize) -> Result<usize> {
    struct Count {
        bytes: usize,
        limit: usize,
    }
    impl io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes) {
                return Err(io::Error::other("activity byte limit"));
            }
            self.bytes += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count { bytes: 0, limit };
    serde_json::to_writer(&mut count, value).map_err(|_| invalid())?;
    Ok(count.bytes)
}

pub(super) fn failed() -> Error {
    Error::http(
        502,
        "远端Agent工具已明确失败，本轮未完成；后续正常结束消息不能清除此事实",
    )
    .with_code("monkeycode_remote_activity_failed")
}

fn invalid() -> Error {
    Error::http(
        502,
        "远端活动字段缺失、矛盾或超限；未回显原始输入或猜测工具语义",
    )
    .with_code("monkeycode_remote_activity_invalid")
}
