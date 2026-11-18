//! CatPaw 有限受控一轮的 OpenAI 出站。只用既有纯解析器，不提前结束上游读取。
//! 响应 ID/时间必须来自本次宿主 prepare，不读取 clock、不合成用量或工具材料。
//! finish/DONE 只在原 Owner 真正 settle 和 close 后下发。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

use crate::{
    errors,
    stream::{Material, ParsedTurn},
    turn_host::PreparedCall,
};

const MAX_OUTPUT: usize = 16 * 1024 * 1024;

pub(super) struct Output {
    id: String,
    model: String,
    created: u64,
    include_usage: bool,
    head: bool,
    ended: bool,
    bytes: usize,
}

impl Output {
    pub(super) fn new(call: &PreparedCall, model: &str, include_usage: bool) -> Self {
        Self {
            id: call.response_id.clone(),
            model: model.to_string(),
            created: call.created,
            include_usage,
            head: false,
            ended: false,
            bytes: 0,
        }
    }

    pub(super) fn begin(&mut self) -> Result<()> {
        if self.head {
            return Err(errors::protocol("CatPaw 重复发送响应头"));
        }
        sdk::stream_head(200).map_err(output_error)?;
        self.head = true;
        self.frame(json!({"role": "assistant"}), None, None, None)
    }

    pub(super) fn material(&mut self, material: Material) -> Result<()> {
        if self.ended {
            return Err(errors::protocol("CatPaw 响应已结束，未重发材料"));
        }
        if material
            .delta
            .as_object()
            .is_none_or(|value| value.is_empty())
        {
            return Ok(());
        }
        // 仅转移真实delta；事件名称和消息结束标记不作为远端收尾或交接许可。
        let Material { delta, event_name, message_finished } = material;
        let _ = (event_name, message_finished);
        self.frame(delta, None, None, None)
    }

    pub(super) fn finish(&mut self, parsed: &ParsedTurn) -> Result<()> {
        if !self.head || self.ended {
            return Err(errors::protocol("CatPaw 响应未开始或已结束"));
        }
        let tools = parsed.message["tool_calls"].as_array().is_some_and(|calls| !calls.is_empty());
        if parsed.message["role"] != "assistant" || !parsed.message["content"].is_string()
            || tools != (parsed.finish_reason == "tool_calls") {
            return Err(errors::protocol("CatPaw 最终消息与结束原因不一致，未发送成功终帧"));
        }
        self.frame(
            json!({}),
            Some(parsed.finish_reason),
            None,
            None,
        )?;
        if self.include_usage {
            if let Some(usage) = &parsed.usage {
                self.frame(Value::Null, None, Some(usage.clone()), None)?;
            }
        }
        self.send(b"data: [DONE]\n\n")?;
        self.ended = true;
        Ok(())
    }

    /// 已发头失败只发错误，不发成功 finish 或 DONE；宿主仍须记录真实失败旁路。
    pub(super) fn failure(&mut self, error: &Error) -> Result<()> {
        if !self.head || self.ended {
            return Ok(());
        }
        let value = json!({"error": {"message": error.message, "type": "upstream_error",
            "code": error.code, "status": error.status, "upstream_code": error.upstream_code}});
        self.ended = true;
        self.event(&value, None)
    }

    fn frame(
        &mut self,
        delta: Value,
        finish: Option<&str>,
        usage: Option<Value>,
        event_name: Option<&str>,
    ) -> Result<()> {
        let choices = if usage.is_some() {
            json!([])
        } else {
            json!([{ "index": 0, "delta": delta, "finish_reason": finish }])
        };
        let mut value = json!({"id": self.id, "object": "chat.completion.chunk", "created": self.created,
            "model": self.model, "choices": choices});
        if let Some(usage) = usage {
            value["usage"] = usage;
        }
        self.event(&value, event_name)
    }

    fn event(&mut self, value: &Value, event_name: Option<&str>) -> Result<()> {
        // 单帧有界；SDK再切成64KiB传输块，不截断真实文本或工具参数。
        crate::prepare::json_bytes(value, 2 * 1024 * 1024)
            .map_err(|_| errors::protocol("CatPaw 单帧响应超过 2 MiB 限制"))?;
        let text =
            serde_json::to_string(value).map_err(|_| errors::protocol("CatPaw 响应帧无法编码"))?;
        let mut frame = String::new();
        if let Some(name) = event_name {
            if name.len() > 128 || name.chars().any(char::is_control) {
                return Err(errors::protocol("CatPaw 出站 SSE event 名格式异常"));
            }
            frame.push_str("event: ");
            frame.push_str(name);
            frame.push('\n');
        }
        frame.push_str("data: ");
        frame.push_str(&text);
        frame.push_str("\n\n");
        self.send(frame.as_bytes())
    }

    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .filter(|bytes| *bytes <= MAX_OUTPUT)
            .ok_or_else(|| errors::protocol("CatPaw 全部响应帧超过 16 MiB"))?;
        sdk::stream_bytes(bytes).map_err(output_error)
    }
}

fn output_error(_: Error) -> Error {
    Error::http(499, "CatPaw 下游响应已关闭或发送未确认，未重发上游轮次")
        .with_code("catpaw_turn_output_unconfirmed")
}
