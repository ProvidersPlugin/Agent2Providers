//! 原生Gemini事件预检：成功HTTP不是成功生成；错误/安全终态/图片不被当文本吞掉。
//! 内容、工具、usage、真实finishReason交由SDK Gemini Converter转回Chat。

use super::{errors, sse::Reader};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub struct Guard {
    pub terminal: bool,
    pub output: bool,
}
impl Guard {
    pub fn new() -> Self {
        Self {
            terminal: false,
            output: false,
        }
    }
    pub fn next(&mut self, reader: &mut Reader) -> Result<Option<Value>> {
        loop {
            let Some(event) = reader.next()? else {
                return Ok(None);
            };
            if event.data.trim() == "[DONE]" {
                if !self.terminal {
                    return Err(incomplete());
                }
                return Ok(None);
            }
            let value: Value = serde_json::from_str(&event.data)
                .map_err(|_| Error::http(502, "Antigravity SSE 事件不是有效 JSON"))?;
            if !value.is_object() {
                return Err(Error::http(502, "Antigravity SSE 事件不是对象"));
            }
            if event.name == "error"
                || errors::failed(&value)
                || errors::failed(errors::inner(&value))
            {
                return Err(errors::upstream(200, &value));
            }
            let inner = errors::inner(&value);
            if errors::content_blocked(&value) {
                return Err(errors::upstream(
                    400,
                    &json!({ "code": "antigravity_content_blocked" }),
                ));
            }
            if let Some(candidates) = inner["candidates"].as_array() {
                if candidates.len() > 1 {
                    return Err(Error::http(
                        502,
                        "Antigravity 返回多个候选，当前单候选对话通道未丢弃其余结果",
                    ));
                }
                for candidate in candidates {
                    if let Some(finish) =
                        candidate["finishReason"].as_str().filter(|v| !v.is_empty())
                    {
                        if !matches!(finish, "STOP" | "MAX_TOKENS") {
                            return Err(Error::http(502, "Antigravity 未正常完成生成或工具调用")
                                .with_code("antigravity_upstream"));
                        }
                        self.terminal = true;
                    }
                    for part in candidate["content"]["parts"]
                        .as_array()
                        .into_iter()
                        .flatten()
                    {
                        if part.get("inlineData").is_some() || part.get("inline_data").is_some() {
                            return Err(Error::http(
                                400,
                                "Antigravity 返回了图片，请使用图片生成/编辑入口；未将图片静默丢弃",
                            )
                            .with_code("local_precondition"));
                        }
                        if part["text"].as_str().is_some_and(|v| !v.is_empty()) {
                            self.output = true;
                        }
                        if let Some(call) = part.get("functionCall") {
                            if call["name"].as_str().is_none_or(|s| s.trim().is_empty()) {
                                return Err(Error::http(
                                    502,
                                    "Antigravity 工具调用缺少名称，未返回伪成功",
                                ));
                            }
                            if call
                                .get("args")
                                .is_some_and(|args| !args.is_null() && !args.is_object())
                            {
                                return Err(Error::http(
                                    502,
                                    "Antigravity 工具调用参数不是完整对象",
                                ));
                            }
                            self.output = true;
                        }
                    }
                }
            }
            return Ok(Some(value));
        }
    }
    pub fn finish(&self) -> Result<()> {
        if self.terminal {
            Ok(())
        } else {
            Err(incomplete())
        }
    }
}
pub fn incomplete() -> Error {
    Error::http(502, "Antigravity 流未返回真实完成原因，响应可能被截断")
        .with_code("antigravity_transient")
}
