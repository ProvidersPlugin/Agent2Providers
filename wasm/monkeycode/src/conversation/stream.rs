//! 正文/思考仅来自实际文本；远端活动只加顶层元信息，失败不冒充正常finish或零usage。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

use super::activity::{self, Activity};

const MAX_OUTPUT: usize = 8 * 1024 * 1024;
const MAX_PENDING_ACTIVITY_BYTES: usize = MAX_OUTPUT;
const MAX_PENDING_ACTIVITIES: usize = 4096;

pub(super) struct Stream {
    id: String,
    model: String,
    created: i64,
    started: bool,
    finished: bool,
    output_bytes: usize,
    pending_activities: Vec<Activity>,
    pending_activity_bytes: usize,
    activity_failed: bool,
    bridge: sdk::tool_bridge::ToolBridge,
    pending_text: String,
}

impl Stream {
    pub(super) fn new(correlation: &str, model: &str, created: i64, bridge: sdk::tool_bridge::ToolBridge) -> Self {
        // 随机宿主资源值仅供本地输出关联，不泄漏或猜造远端task UUID。
        Self {
            id: format!("chatcmpl-{correlation}"),
            model: model.to_string(),
            created,
            started: false,
            finished: false,
            output_bytes: 0,
            pending_activities: Vec::new(),
            pending_activity_bytes: 0,
            activity_failed: false,
            bridge,
            pending_text: String::new(),
        }
    }

    pub(super) fn text(&mut self, thought: bool, text: &str) -> Result<()> {
        if self.finished
            || text.is_empty()
            || text.len() > MAX_OUTPUT.saturating_sub(self.output_bytes)
        {
            return Err(Error::http(502, "任务输出状态或预算异常，未截断成成功")
                .with_code("monkeycode_task_output_invalid"));
        }
        self.output_bytes += text.len();
        if self.bridge.active() && !thought { self.pending_text.push_str(text); return Ok(()); }
        self.head()?;
        let delta = if thought {
            json!({ "reasoning_content": text })
        } else {
            json!({ "content": text })
        };
        sdk::stream_event(&self.frame(delta, Value::Null))
    }

    /// 只读活动不建立成功head、不成为正文或思考；已有head时只加顶层自定义信息。
    pub(super) fn activity(&mut self, activity: Activity) -> Result<()> {
        self.activity_failed |= activity.failed();
        if self.finished {
            return Err(Error::http(502, "任务活动在终态之后到达，未继续输出")
                .with_code("monkeycode_task_output_invalid"));
        }
        let bytes = activity.read_bytes();
        if bytes > MAX_OUTPUT.saturating_sub(self.output_bytes) {
            return Err(
                Error::http(502, "任务文本与活动超过共同输出预算，未截断成成功")
                    .with_code("monkeycode_task_output_invalid"),
            );
        }
        self.output_bytes += bytes;
        if self.started {
            return self.activity_frame(&activity);
        }
        let bytes = activity.metadata_bytes();
        if self.pending_activities.len() >= MAX_PENDING_ACTIVITIES
            || bytes > MAX_PENDING_ACTIVITY_BYTES.saturating_sub(self.pending_activity_bytes)
        {
            return Err(Error::http(
                502,
                "成功响应之前的只读活动超过缓存预算，未提前宣称回答成功",
            )
            .with_code("monkeycode_task_activity_limit"));
        }
        self.pending_activity_bytes += bytes;
        self.pending_activities.push(activity);
        Ok(())
    }

    /// 仅主消费者收到真实round_ended才调用；已见工具失败不能被后续正常终态清除。
    pub(super) fn finish(&mut self) -> Result<()> {
        if self.activity_failed {
            return Err(activity::failed());
        }
        if self.finished {
            return Err(Error::http(502, "任务重复终态，未再次输出结束帧")
                .with_code("monkeycode_task_duplicate_end"));
        }
        if !self.started && self.pending_text.is_empty() {
            return Err(Error::http(502, "MonkeyCode 任务已结束，但没有返回回答或思考内容")
                .with_code("monkeycode_task_empty_output"));
        }
        let message = self.bridge.message(&self.pending_text)?;
        self.head()?;
        let finish = if self.bridge.active() {
            if let Some(calls) = message["tool_calls"].as_array() {
                let calls: Vec<Value> = calls.iter().enumerate().map(|(index,call)| {
                    let mut call = call.clone(); call["index"] = json!(index); call
                }).collect();
                sdk::stream_event(&self.frame(json!({"tool_calls":calls}),Value::Null))?;
                "tool_calls"
            } else {
                sdk::stream_event(&self.frame(json!({"content":message["content"]}),Value::Null))?;
                "stop"
            }
        } else { "stop" };
        sdk::stream_event(&self.frame(json!({}), json!(finish)))?;
        sdk::stream_bytes(b"data: [DONE]\n\n")?;
        self.finished = true;
        Ok(())
    }

    pub(super) fn error(&mut self, error: &Error) -> Result<()> {
        if !self.started || self.finished {
            return Ok(());
        }
        self.flush_activities()?;
        self.finished = true;
        // 错误只输出error帧并让调用真实失败；不发正常finish/[DONE]，不合成usage。
        sdk::stream_event(&json!({
            "error": {
                "message": error.message,
                "status": error.status,
                "code": error.code,
                "upstream_code": error.upstream_code,
                "type": "upstream_error",
                "retry_allowed": false,
            }
        }))
    }

    fn head(&mut self) -> Result<()> {
        if self.started {
            return Ok(());
        }
        sdk::stream_head(200)?;
        self.started = true;
        sdk::stream_event(&self.frame(json!({ "role": "assistant" }), Value::Null))?;
        self.flush_activities()
    }

    fn flush_activities(&mut self) -> Result<()> {
        let activities = std::mem::take(&mut self.pending_activities);
        self.pending_activity_bytes = 0;
        for activity in activities {
            self.activity_frame(&activity)?;
        }
        Ok(())
    }

    fn activity_frame(&self, activity: &Activity) -> Result<()> {
        let mut frame = self.frame(json!({}), Value::Null);
        // 顶层自定义只读观察信息；delta保持空，不发tool_calls，也不发原始input。
        frame["agent_activity"] = activity.projection();
        sdk::stream_event(&frame)
    }

    fn frame(&self, delta: Value, finish: Value) -> Value {
        json!({
            "id": self.id, "object": "chat.completion.chunk", "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        })
    }
}
