//! CatPaw 固定轮次的纯出站转换，只由同包受控消费者使用，不自行联网。
//! 协议依据：原生 catpaw/openai.rs（消息/错误信封、累计字段、usage），
//! upstream_http.rs（turn 为 SSE）、turn_executor.rs（finished 不是 turn 结束）。
//! 与旧实现有意不同：坏事件、非前缀覆盖、缺终态均失败；不补零 usage。
//! 所有返回值只是本地观察材料，不是创建、停止、交接或远端完成收据。

use agent2api_wasm_sdk::{json, Error, Result, Value};

use crate::{
    errors,
    prepare::Prepared,
    stream_snapshot as snapshot,
    stream_sse::{Event, Reader},
    tools::ToolChoice,
};

const MAX_OUTPUT: usize = 8 * 1024 * 1024;
const MAX_TOTAL_NODES: usize = 1_048_576;

/// event 名称原样保留；delta 仅描述消息变化，不代表任何工具已执行。
pub(super) struct Material {
    pub(super) event_name: Option<String>,
    pub(super) delta: Value,
    /// 只标记本消息结束；调用方不得据此关流或向上游回报 completed。
    pub(super) message_finished: bool,
}

/// 只有完整 DONE、真实消息 finished=true 且读取正常收尾才构造。
/// tools 非空仍只是等待受控交接的调用材料，不能当 conversation 已完成。
pub(super) struct ParsedTurn {
    pub(super) message: Value,
    pub(super) upstream_message: Value,
    pub(super) observed_conversation_id: Option<String>,
    /// 原始 SSE event 名，缺失与显式空串区分；不把它当远端完成证明。
    pub(super) done_event_name: Option<String>,
    pub(super) usage: Option<Value>,
    pub(super) finish_reason: &'static str,
}

pub(super) struct Stream {
    reader: Reader,
    state: State,
    failure: Option<Error>,
    ended: bool,
}

struct State {
    expected_conversation: String,
    observed_conversation: Option<String>,
    declared: Vec<String>,
    choice: ToolChoice,
    last: Option<snapshot::Snapshot>,
    usage: Option<Value>,
    done: bool,
    done_event_name: Option<String>,
    output_bytes: usize,
    nodes: usize,
}

impl Stream {
    /// Prepared 仅提供本次请求约束和关联文本；不授予发送许可、不证明远端对象存在。
    pub(super) fn new(prepared: &Prepared) -> Self {
        Self {
            reader: Reader::new(),
            failure: None,
            ended: false,
            state: State {
                expected_conversation: prepared.correlation().0.to_string(),
                observed_conversation: None,
                declared: prepared.tools().available.clone(),
                choice: prepared.tools().choice.clone(),
                last: None,
                usage: None,
                done: false,
                done_event_name: None,
                output_bytes: 0,
                nodes: 0,
            },
        }
    }

    /// 不调用宿主，不生成 OpenAI id/时间；一个输入块失败时不交付该块的部分产物。
    /// 已交付的旧增量不可撤回，所以任何错误都锁定本解析器，不允许继续或重试。
    pub(super) fn push(&mut self, bytes: &[u8]) -> Result<Vec<Material>> {
        self.available()?;
        let mut output = Vec::new();
        let state = &mut self.state;
        let result = self
            .reader
            .push(bytes, |event| state.consume(event, &mut output));
        if let Err(error) = result {
            self.failure = Some(error.clone());
            return Err(error);
        }
        Ok(output)
    }

    /// 必须在本次宿主真实正常读取结束时调用；网络错误/取消不能转调此入口。
    /// 不采用原生的 EOF 兜底成功：无完整 DONE 即使 finished=true 仍明确失败。
    pub(super) fn finish(&mut self) -> Result<ParsedTurn> {
        self.available()?;
        self.ended = true;
        let state = &mut self.state;
        // EOF 尾部只允许检查错误；非完整 SSE 事件不追加为正常数据或 DONE。
        if let Err(error) = self
            .reader
            .finish(|event| state.consume(event, &mut Vec::new()))
        {
            self.failure = Some(error.clone());
            return Err(error);
        }
        if !state.done {
            return Err(errors::protocol(
                "CatPaw 流缺少完整 DONE 终态，未把断流记为成功",
            ));
        }
        let last = state
            .last
            .take()
            .filter(|last| last.finished)
            .ok_or_else(|| errors::protocol("CatPaw 终态缺少真实已结束消息"))?;
        let mut message = json!({"role": "assistant", "content": last.text});
        if !last.reasoning.is_empty() {
            message["reasoning_content"] = json!(last.reasoning);
        }
        let finish_reason = if last.calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        };
        if !last.calls.is_empty() {
            message["tool_calls"] = Value::Array(
                last.calls
                    .iter()
                    .map(|call| {
                        json!({
                            "id": call.id, "type": "function",
                            "function": {"name": call.name, "arguments": call.arguments}
                        })
                    })
                    .collect(),
            );
        }
        state.charge(&message)?;
        state.charge(&last.raw)?;
        if let Some(usage) = state.usage.take() {
            state.charge(&usage)?;
            Ok(ParsedTurn {
                message,
                upstream_message: last.raw,
                observed_conversation_id: state.observed_conversation.take(),
                done_event_name: state.done_event_name.take(),
                usage: Some(usage),
                finish_reason,
            })
        } else {
            Ok(ParsedTurn {
                message,
                upstream_message: last.raw,
                observed_conversation_id: state.observed_conversation.take(),
                done_event_name: state.done_event_name.take(),
                usage: None,
                finish_reason,
            })
        }
    }

    /// 纯粹锁定本地解析器；不发 stop、不安排清理、不把取消伪装成正常 EOF。
    pub(super) fn interrupted(&mut self) -> Error {
        let error = self
            .failure
            .clone()
            .unwrap_or_else(|| errors::protocol("CatPaw 上游流中断或被取消，未确认完整终态"));
        self.failure = Some(error.clone());
        error
    }

    fn available(&self) -> Result<()> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if self.ended {
            return Err(errors::protocol("CatPaw 出站解析已收尾，不能重复消费"));
        }
        Ok(())
    }
}

impl State {
    fn consume(&mut self, event: Event, output: &mut Vec<Material>) -> Result<()> {
        // 必须先判命名错误：event:error + data:[DONE] 也是失败，不先认 DONE。
        if event
            .name
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case("error"))
        {
            return Err(errors::protocol("CatPaw 上游发送了命名 error 事件"));
        }
        let raw = event.data.trim();
        if raw == "[DONE]" {
            self.check_event_name(event.name.as_deref())?;
            if !event.complete || self.done || !self.last.as_ref().is_some_and(|last| last.finished)
            {
                return Err(errors::protocol("CatPaw DONE 不完整、重复或先于消息结束"));
            }
            self.done = true;
            self.charge(&json!({"event": event.name, "doneObserved": true}))?;
            self.done_event_name = event.name;
            // 不在这里发成功结果；继续检查同块尾部，finish 才产本地完整结果。
            return Ok(());
        }
        if raw.is_empty() {
            return Err(errors::protocol("CatPaw SSE 事件缺少有效数据"));
        }
        let parsed: Value = serde_json::from_str(raw)
            .map_err(|_| errors::protocol("CatPaw SSE 数据不是完整 JSON，未静默跳过"))?;
        self.nodes = self
            .nodes
            .checked_add(snapshot::nodes(&parsed)?)
            .filter(|count| *count <= MAX_TOTAL_NODES)
            .ok_or_else(|| errors::protocol("CatPaw 上游流超过累计节点预算"))?;
        check_errors(&parsed, 0)?;
        // 正文里的 error/code 先于终态、未知事件与 EOF 判断，避免吞掉原始失败类别。
        self.check_event_name(event.name.as_deref())?;
        if !event.complete {
            return Err(errors::protocol("CatPaw EOF 留下未结束的 SSE 事件"));
        }
        if self.done {
            return Err(errors::protocol("CatPaw DONE 后仍有数据，未提前确认成功"));
        }
        let object = parsed
            .as_object()
            .ok_or_else(|| errors::protocol("CatPaw 上游事件信封不是对象"))?;
        let payload = object.get("data").unwrap_or(&parsed);
        if !payload.is_object() {
            return Err(errors::protocol("CatPaw 上游 data 信封不是已支持的对象"));
        }
        self.observe_conversation(&parsed)?;
        self.observe_conversation(payload)?;
        for field in ["data", "result"] {
            if let Some(nested) = payload.get(field) {
                if !nested.is_object() {
                    return Err(errors::protocol("CatPaw 上游嵌套信封不是已支持的对象"));
                }
                self.observe_conversation(nested)?;
            }
        }
        let message = unique_message(payload)?;
        let usage = snapshot::usage(payload)?;
        // 无消息的状态/心跳形态没有足够证据，不猜状态名，也不让未知事件形成成功。
        if message.is_none() && usage.is_none() {
            return Err(errors::protocol(
                "CatPaw 上游事件没有已支持的消息或完整用量，未猜测控制语义",
            ));
        }
        if let Some(message) = message {
            let next = snapshot::Snapshot::parse(message)?;
            let delta = next.delta(self.last.as_ref(), &self.declared, &self.choice)?;
            let material_bytes =
                json!({"event": event.name, "delta": delta, "messageFinished": next.finished});
            self.charge(&material_bytes)?;
            output.push(Material {
                event_name: event.name,
                delta,
                message_finished: next.finished,
            });
            self.last = Some(next);
        } else {
            // 用量事件不生成伪文本，但保留其实际 event 名；无任何执行/收尾语义。
            self.charge(&json!({"event": event.name, "delta": {}, "messageFinished": false}))?;
            output.push(Material {
                event_name: event.name,
                delta: json!({}),
                message_finished: false,
            });
        }
        // 缺失/不完整用量保持未知，不沿用更早事件的旧值，更不合成三个零。
        self.usage = usage;
        Ok(())
    }

    fn check_event_name(&self, name: Option<&str>) -> Result<()> {
        match name {
            None | Some("" | "message") => Ok(()),
            _ => Err(errors::protocol(
                "CatPaw SSE 命名事件尚无本协议转换依据，未猜测其含义",
            )),
        }
    }

    fn observe_conversation(&mut self, payload: &Value) -> Result<()> {
        if let Some(value) = payload.get("conversationId") {
            let id = snapshot::identifier(value)?;
            if id != self.expected_conversation {
                return Err(errors::protocol(
                    "CatPaw 上游 conversationId 与本次关联材料不一致",
                ));
            }
            self.observed_conversation = Some(id.to_string());
        }
        Ok(())
    }

    fn charge(&mut self, value: &Value) -> Result<()> {
        let size = crate::prepare::json_bytes(value, MAX_OUTPUT)
            .map_err(|_| errors::protocol("CatPaw 出站材料超过大小限制"))?;
        self.output_bytes = self
            .output_bytes
            .checked_add(size)
            .filter(|size| *size <= MAX_OUTPUT)
            .ok_or_else(|| errors::protocol("CatPaw 流式增量与最终材料合计超过 8 MiB"))?;
        Ok(())
    }
}

/// 原生 response_message 的三个位置；多处同时存在必须完全一致，不按优先级掩盖冲突。
fn unique_message(payload: &Value) -> Result<Option<&Value>> {
    let mut found = None;
    for value in [
        payload.get("message"),
        payload.pointer("/data/message"),
        payload.pointer("/result/message"),
    ]
    .into_iter()
    .flatten()
    {
        if found.is_some_and(|previous| previous != value) {
            return Err(errors::protocol("CatPaw 上游消息信封存在冲突"));
        }
        found = Some(value);
    }
    Ok(found)
}

/// 原生 unwrap_api_data / response_error 的 code、error、data.error、result.error。
/// 比旧版更严格：非数字成功码与非空异常 error 不忽略；仅输出固定文案/数字码。
fn check_errors(value: &Value, depth: usize) -> Result<()> {
    let Some(object) = value.as_object() else {
        return Ok(());
    };
    if depth > 4 {
        return Err(errors::protocol("CatPaw 上游错误信封嵌套过深"));
    }
    if let Some(error) = object.get("error").filter(|error| !error.is_null()) {
        return Err(upstream_error(error));
    }
    if object.get("type").and_then(Value::as_str) == Some("error") {
        return Err(upstream_error(value));
    }
    if let Some(code) = object.get("code") {
        match code.as_i64() {
            Some(0 | 200) => {}
            Some(_) => return Err(upstream_error(value)),
            None => return Err(errors::protocol("CatPaw 上游业务码不是已知数字成功码")),
        }
    }
    for field in ["data", "result"] {
        if let Some(nested) = object.get(field) {
            check_errors(nested, depth + 1)?;
        }
    }
    Ok(())
}

fn upstream_error(value: &Value) -> Error {
    let status = value["httpStatus"]
        .as_u64()
        .and_then(|status| u16::try_from(status).ok())
        .filter(|status| (400..600).contains(status))
        .unwrap_or(502);
    let mut error = Error::http(status, "CatPaw 上游流返回业务错误，未确认本轮成功")
        .with_code("catpaw_stream_upstream");
    error.upstream_code = value["code"].as_i64();
    error
}
