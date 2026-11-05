//! MonkeyCode 任务流的**下行翻译层**：ACP 事件 → 本网关的 OpenAI 兼容帧。
//!
//! ── 与 `stream.rs` 的分工 ─────────────────────────────────────
//! `stream.rs` 管连接与驱动（WS 握手、起始消息、超时、帧出口）；本文件是纯
//! 状态机：一条下行 JSON 帧进，零到多条 `chat.completion.chunk` 出，外加
//! 「要不要回过一条控制消息」（ping / reply-question）与「是不是终态」。
//! 拆文件只是因为单文件行数约定；两者是同一段时序的两半。
//!
//! ── ACP 事件映射（逐条对照 `task-runner.ts::handleACPEvent`）──────
//! | ACP 事件 | 产出 |
//! |---|---|
//! | `agent_message_chunk` | `delta.content`（`text` 优先、`content` 兜底）|
//! | `agent_thought_chunk` | `delta.reasoning_content`（本仓思考通道；参考是 `[Thinking] …` 拼正文）|
//! | `tool_call` | `delta.tool_calls[{index,id,type:"function",function:{name,arguments}}]`（参考 Responses 分支的 function_call 形态；chat 分支参考拼正文，本仓按工具调用形态下发）|
//! | `tool_call_update` | 参数增量的 `tool_calls[{index,function:{arguments}}]` 帧（参考只记日志；增量语义见 `06-acp-event-reference`）|
//! | `usage_update` | 并入 usage 计数（按累计值处理，非零才覆盖）|
//! | `plan` / `available_commands_update` | 仅 verbose 日志（参考同）|
//! | 未知 | 仅 verbose 日志 |
//!
//! 下行信封（`ping` / `task-started` / `task-running` / `task-ended` /
//! `task-error` / 上游真实错误帧 `error` / `permission-req` / `task-event`）
//! 的处理与四个终态出口见 `stream.rs` 的模块头。
//!
//! ── `data` 一律是 base64(JSON)（本家最容易踩的坑）────────────────
//! 线上每个帧的 `data` 都是 base64(JSON)（上游 `domain.TaskStream.Data` 是
//! Go 的 `[]byte`）。下行解码走 `wire::decode_data`、上行封帧走
//! `wire::uplink_frame`，约定与出处见 `wire.rs` 的模块头 —— 别在本文件里另写
//! 一份（历史上就是写成「裸 JSON 字符串」，导致一帧都解不出）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic，不碰网络、不持锁。

use bytes::Bytes;
use serde_json::{json, Map, Value};

use crate::logging;

use super::wire::{decode_data, uplink_frame};

/// `data: [DONE]\n\n`（OpenAI 流收尾帧；`stream.rs` 的异常收尾也用）
pub(super) const DONE_FRAME: &[u8] = b"data: [DONE]\n\n";

// ─── 翻译状态机 ────────────────────────────────────────────────

/// 一次工具调用的累积状态（ACP 的 `tool_call` / `tool_call_update`）。
struct ToolCallState {
    id: String,
    name: String,
    arguments: String,
}

/// Token 用量（报告口径：OpenAI 的三件套）。
#[derive(Default, Clone, Copy)]
struct Usage {
    input: i64,
    output: i64,
    total: i64,
}

impl Usage {
    fn seen(&self) -> bool {
        self.input != 0 || self.output != 0 || self.total != 0
    }

    fn to_json(&self) -> Value {
        json!({
            "prompt_tokens": self.input,
            "completion_tokens": self.output,
            "total_tokens": self.total,
        })
    }
}

/// 一个任务流的下行翻译状态机（流式 / 非流式共用）。
///
/// 上游的每个 ACP 事件在这里变成 OpenAI chunk；非流式收尾时同一份累积状态
/// （`content` / `reasoning` / `tools`）直接组装 `chat.completion`。
pub struct Translator {
    /// 面向客户端的响应 id（`chatcmpl-<taskId>`，参考同款）
    chat_id: String,
    /// 客户端请求的模型名（下发帧的 `model` 字段）
    model: String,
    /// 客户端是否要 usage 帧（`stream_options.include_usage`）
    include_usage: bool,
    content: String,
    reasoning: String,
    tools: Vec<ToolCallState>,
    /// 最近一次工具调用的下标（`tool_call_update` 的增量参数挂到它上面）
    last_tool_index: Option<usize>,
    usage: Usage,
    role_sent: bool,
    finished: bool,
}

impl Translator {
    pub(super) fn new(chat_id: String, model: String, include_usage: bool) -> Self {
        Self {
            chat_id,
            model,
            include_usage,
            content: String::new(),
            reasoning: String::new(),
            tools: Vec::new(),
            last_tool_index: None,
            usage: Usage::default(),
            role_sent: false,
            finished: false,
        }
    }

    /// 首帧：`delta.role = assistant`（本仓约定；参考没有这一帧，OpenAI
    /// 客户端按它建立 assistant 消息，早发比晚发稳）。只发一次。
    pub(super) fn role_frame(&mut self) -> Option<Bytes> {
        if self.role_sent {
            return None;
        }
        self.role_sent = true;
        Some(self.chunk_frame(json!({ "role": "assistant" }), None))
    }

    /// 一帧 `chat.completion.chunk`。
    fn chunk_frame(&self, delta: Value, finish_reason: Option<&str>) -> Bytes {
        let choice = json!({
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        });
        sse_bytes(&json!({
            "id": self.chat_id,
            "object": "chat.completion.chunk",
            "created": logging::now_ms() / 1000,
            "model": self.model,
            "choices": [choice],
        }))
    }

    /// usage 帧（OpenAI 收尾形态：`choices` 为空数组）
    fn usage_frame(&self) -> Bytes {
        sse_bytes(&json!({
            "id": self.chat_id,
            "object": "chat.completion.chunk",
            "created": logging::now_ms() / 1000,
            "model": self.model,
            "choices": [],
            "usage": self.usage.to_json(),
        }))
    }

    /// 流内错误帧（HTTP 头早已发出，只能这样告诉客户端这次补全失败）。
    pub(super) fn error_frame(&self, message: &str) -> Bytes {
        sse_bytes(&json!({
            "id": self.chat_id,
            "object": "chat.completion.chunk",
            "created": logging::now_ms() / 1000,
            "model": self.model,
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
            "error": { "message": message, "type": "upstream_error" },
        }))
    }

    /// 正常收尾：`finish` 帧 +（客户端要的话）usage 帧 + `[DONE]`（幂等）。
    pub(super) fn finish_frames(&mut self) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut frames = vec![self.chunk_frame(json!({}), Some("stop"))];
        if self.include_usage {
            frames.push(self.usage_frame());
        }
        frames.push(Bytes::from_static(DONE_FRAME));
        frames
    }

    /// 正文增量
    fn push_content(&mut self, text: &str) -> Bytes {
        self.content.push_str(text);
        self.chunk_frame(json!({ "content": text }), None)
    }

    /// 思考增量（本仓走 reasoning 通道，见模块头）
    fn push_reasoning(&mut self, text: &str) -> Bytes {
        self.reasoning.push_str(text);
        self.chunk_frame(json!({ "reasoning_content": text }), None)
    }

    /// 工具调用（一次事件给全量参数；`index` 是客户端配对用的下标）
    fn push_tool_call(&mut self, name: &str, input: &str) -> Bytes {
        let index = self.tools.len();
        // id 用任务 id（去掉 `chatcmpl-` 前缀）拼 idx：ACP 的 tool_call 事件
        // 没有携带 id，客户端做 tool_use/tool_result 配对时需要稳定且唯一的值。
        let task = self.chat_id.strip_prefix("chatcmpl-").unwrap_or(&self.chat_id);
        let id = format!("call_{index}_{task}");
        self.tools.push(ToolCallState {
            id: id.clone(),
            name: name.to_string(),
            arguments: input.to_string(),
        });
        self.last_tool_index = Some(index);
        self.chunk_frame(
            json!({
                "tool_calls": [{
                    "index": index,
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": input },
                }],
            }),
            None,
        )
    }

    /// 工具参数的增量更新（`tool_call_update`）：只发 `arguments` 增量，
    /// 客户端按 index 累积成完整参数（OpenAI 流式工具调用的标准形态）。
    fn append_tool_arguments(&mut self, tool_name: &str, delta: &str) -> Option<Bytes> {
        if delta.is_empty() {
            return None;
        }
        let index = if tool_name.is_empty() {
            self.last_tool_index?
        } else {
            self.tools
                .iter()
                .rposition(|tool| tool.name == tool_name)?
        };
        let tool = self.tools.get_mut(index)?;
        tool.arguments.push_str(delta);
        self.last_tool_index = Some(index);
        Some(self.chunk_frame(
            json!({
                "tool_calls": [{
                    "index": index,
                    "function": { "arguments": delta },
                }],
            }),
            None,
        ))
    }

    /// 收尾时要上报给编排层记账的 usage（没有收到任何非零计数时 None ——
    /// 失败 / 无用量路径不杜撰 token，见 `stream.rs` 的收尾）。
    #[allow(dead_code)]
    pub(super) fn reportable_usage(&self) -> Option<Value> {
        if self.usage.seen() {
            Some(self.usage.to_json())
        } else {
            None
        }
    }

    /// 收尾日志用的一句话摘要（只记长度、不落正文 —— 与 CatPaw 同一隐私口径）。
    pub(super) fn summary(&self) -> String {
        format!(
            "正文 {} 字、思考 {} 字、工具 {} 个",
            self.content.chars().count(),
            self.reasoning.chars().count(),
            self.tools.len(),
        )
    }

    /// 非流式的完整响应体（`chat.completion`）。
    pub(super) fn completion_body(&self) -> Value {
        let mut message = Map::new();
        message.insert("role".to_string(), Value::String("assistant".to_string()));
        message.insert(
            "content".to_string(),
            if self.content.is_empty() {
                Value::Null
            } else {
                Value::String(self.content.clone())
            },
        );
        if !self.reasoning.is_empty() {
            message.insert(
                "reasoning_content".to_string(),
                Value::String(self.reasoning.clone()),
            );
        }
        if !self.tools.is_empty() {
            let tools: Vec<Value> = self
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "id": tool.id,
                        "type": "function",
                        "function": { "name": tool.name, "arguments": tool.arguments },
                    })
                })
                .collect();
            message.insert("tool_calls".to_string(), Value::Array(tools));
        }
        let usage = if self.usage.seen() {
            self.usage.to_json()
        } else {
            json!({ "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 })
        };
        json!({
            "id": self.chat_id,
            "object": "chat.completion",
            "created": logging::now_ms() / 1000,
            "model": self.model,
            "choices": [{
                "index": 0,
                "message": Value::Object(message),
                "finish_reason": "stop",
            }],
            "usage": usage,
        })
    }
}

/// 一条 SSE 帧的字节形态（`data: <json>\n\n`）
fn sse_bytes(value: &Value) -> Bytes {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    Bytes::from(format!("data: {text}\n\n"))
}

// ─── 下行消息处理 ──────────────────────────────────────────────

/// 任务流的终态。
pub(super) enum Terminal {
    /// 正常结束（`task-ended`）
    Ended,
    /// 异常结束（原因进错误帧与请求日志）
    Failed(String),
}

/// 处理一条下行消息的结果。
#[derive(Default)]
pub(super) struct Handled {
    /// 要下发给客户端的 SSE 帧
    pub(super) frames: Vec<Bytes>,
    /// 要回给上游的控制消息（ping / reply-question / permission-resp）
    pub(super) reply: Option<String>,
    /// 终态（None = 继续）
    pub(super) terminal: Option<Terminal>,
    /// 心跳和历史游标不算模型进展，不能延后无输出超时。
    pub(super) progress: bool,
}

// ─── 下行消息处理 ──────────────────────────────────────────────

/// 一条下行文本帧 → 帧 / 回复 / 终态。
pub(super) fn handle_message(translator: &mut Translator, raw: &str) -> Handled {
    let parsed = match serde_json::from_str::<Value>(raw) {
        Ok(value) => value,
        Err(_) => {
            // 参考同样忽略非 JSON 消息（`catch { /* 忽略 */ }`）
            logging::verbose("[MonkeyCode]", "忽略任务流里的非 JSON 消息");
            return Handled::default();
        }
    };
    let frame_type = parsed.get("type").and_then(Value::as_str).unwrap_or("");
    let mut handled = match frame_type {
        // 心跳：回一条 ping（参考 `task-runner.ts` 的 `msg.type === "ping"`）
        "ping" => Handled {
            reply: Some(json!({ "type": "ping" }).to_string()),
            ..Handled::default()
        },
        "task-started" => {
            logging::verbose("[MonkeyCode]", "任务轮次开始（task-started）");
            Handled::default()
        }
        "task-running" => handle_running(translator, &parsed),
        "task-ended" => {
            merge_final_usage(translator, parsed.get("data"));
            Handled {
                frames: translator.finish_frames(),
                terminal: Some(Terminal::Ended),
                ..Handled::default()
            }
        }
        // 终态错误：上游常量是 `error`（`consts.TaskStreamTypeError`，由
        // `task.go::writeError` 发出），`task-error` 是信封层曾用过的名字 ——
        // 两个都认，都走异常收尾（只认其中一个会让真实错误被当未知帧丢掉）。
        "task-error" | "error" => {
            let detail = error_detail(parsed.get("data"));
            Handled {
                terminal: Some(Terminal::Failed(detail)),
                ..Handled::default()
            }
        }
        // 逐工具权限审批：必须回一条 `permission-resp`，否则任务停在工具调用处
        // （官网也一直转圈）。见 `handle_permission_request`。
        "permission-req" => handle_permission_request(parsed.get("data")),
        // 临时事件（不持久化）：无终态语义，只记一条 verbose 便于抓日志定位。
        "task-event" => {
            log_task_event(&parsed);
            Handled::default()
        }
        other => {
            // 其余（file-change / cursor / sync-my-ip / call 等）无终态语义：
            // 记下类型名即可（`cursor`/`file-change` 只在历史回放里有意义，
            // `call`/`call-response` 走的是另一条控制流 WebSocket，不在本流）。
            logging::verbose(
                "[MonkeyCode]",
                &format!("忽略任务流消息类型：{other}"),
            );
            Handled::default()
        }
    };
    handled.progress = handled.progress || !handled.frames.is_empty()
        || frame_type == "task-started"
        || (frame_type != "ping" && handled.reply.is_some());
    handled
}

/// `permission-req` → `permission-resp`（自动批准）。
///
/// ── 载荷形状（逐字对照上游）───────────────────────────────────
/// 下行 `data`（base64(JSON)，解码后）：`{id, title, tool, tool_call_id}`
/// —— 见 `desktop/fixtures/replay/raw.jsonl` 的样例与
/// `desktop/src/driver/frame.rs::build("permission-req", ...)`。
/// 上行 `data`（由 `uplink_frame` 再包 base64）：`{id, approved, remember, persist}`
/// —— 见 `desktop/ui-next/src/lib/ipc/approvals.ts::sendPermAnswerVia` 的
/// `allow` 档（`approved: true, remember: false, persist: false`）与壳侧
/// `desktop/src/driver/session.rs` 的 `"permission-resp"` 分支。
///
/// 缺 `id` 时**不回**（对不上号的批准没有意义），只记一条 verbose。
fn handle_permission_request(data: Option<&Value>) -> Handled {
    let payload = decode_data(data);
    let id = payload
        .as_ref()
        .and_then(|value| value.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if id.is_empty() {
        logging::verbose("[MonkeyCode]", "permission-req 缺少 id，未自动批准");
        return Handled::default();
    }
    let tool = payload
        .as_ref()
        .and_then(|value| value.get("tool"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let title = payload
        .as_ref()
        .and_then(|value| value.get("title"))
        .and_then(Value::as_str)
        .unwrap_or("");
    logging::verbose(
        "[MonkeyCode]",
        &format!(
            "自动批准工具调用：tool={} title={}",
            if tool.is_empty() { "-" } else { tool },
            preview(title, 120),
        ),
    );
    Handled {
        reply: Some(uplink_frame(
            "permission-resp",
            &json!({
                "id": id,
                "approved": true,
                "remember": false,
                "persist": false,
            }),
        )),
        ..Handled::default()
    }
}

/// `task-event` 的 verbose：只记 kind / 摘要（该帧不持久化，也不该产帧）。
fn log_task_event(message: &Value) {
    let payload = decode_data(message.get("data"));
    let kind = payload
        .as_ref()
        .and_then(|value| value.get("kind"))
        .and_then(Value::as_str)
        .or_else(|| message.get("kind").and_then(Value::as_str))
        .filter(|kind| !kind.is_empty())
        .unwrap_or("-")
        .to_string();
    let summary = payload.map(|value| value.to_string()).unwrap_or_default();
    logging::verbose(
        "[MonkeyCode]",
        &format!("任务临时事件 task-event kind={kind} {}", preview(&summary, 200)),
    );
}

/// `task-running`：ACP 事件（转帧）或 Agent 提问（自动回复）。
fn handle_running(translator: &mut Translator, message: &Value) -> Handled {
    match message.get("kind").and_then(Value::as_str).unwrap_or("") {
        "acp_event" => {
            let Some(acp) = decode_data(message.get("data")) else {
                logging::verbose(
                    "[MonkeyCode]",
                    "acp_event 的 data 无法解码（期望 base64(JSON)）",
                );
                return Handled::default();
            };
            handle_acp(translator, &acp)
        }
        "acp_ask_user_question" => {
            let payload = decode_data(message.get("data"));
            let reply = auto_reply_question(payload.as_ref());
            if reply.is_none() {
                logging::verbose(
                    "[MonkeyCode]",
                    "Agent 提问无法解析，未自动回复（任务可能停在提问处）",
                );
            }
            Handled {
                reply,
                ..Handled::default()
            }
        }
        other => {
            logging::verbose(
                "[MonkeyCode]",
                &format!("忽略 task-running kind={other}"),
            );
            Handled::default()
        }
    }
}

/// ACP 事件的**形状归一**：把上游两套形态都转成本文件认识的扁平形态。
///
/// ── 为什么有两套 ────────────────────────────────────────────
/// 参考仓库 `codkeep/MonkeyCodeReverseEngineer` 从真实抓包里看到的是**扁平**形态
/// （`{"type":"agent_message_chunk","text":"…"}`，见其
/// `docs/10-appendices/07-websocket-frames.md` 那条 base64 抓包），本插件一直按它写；
/// 而当前上游开源仓库网页端 `task-message-handler.ts::applyACPEvent` 读的是带
/// `update` 外壳的形态（`data.update.sessionUpdate` / `data.update.content.text`）。
/// 两套字段名完全不兼容，这里都认 —— 上游换版时不至于整条链失灵。
///
/// 已是扁平形态（没有 `update` 外壳）时返回 `None`（调用方原样使用，零拷贝）。
fn normalize_acp(acp: &Value) -> Option<Value> {
    let update = acp.get("update").filter(|value| value.is_object())?;
    let session_update = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .filter(|kind| !kind.is_empty())?;
    match session_update {
        "agent_message_chunk" | "agent_thought_chunk" => {
            let text = update
                .get("content")
                .filter(|content| content.get("type").and_then(Value::as_str) == Some("text"))
                .and_then(|content| content.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("");
            Some(json!({ "type": session_update, "text": text }))
        }
        "tool_call" | "tool_call_update" => {
            let name = update
                .get("title")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .or_else(|| update.get("kind").and_then(Value::as_str))
                .unwrap_or("unknown");
            let input = text_field(update.get("rawInput"));
            Some(json!({
                "type": session_update,
                "tool_name": name,
                "tool_input": input.clone(),
                "delta": input,
                "status": update.get("status").cloned().unwrap_or(Value::Null),
            }))
        }
        // usage_update 在外壳形态里是「上下文窗口 size/used」（不是 token 计数），
        // 与扁平形态的 input/output/total 语义不同 —— 这里只回类型名，不臆造 token。
        _ => Some(json!({ "type": session_update })),
    }
}

/// 一个 ACP 事件 → 帧（映射表见模块头与报告）。
fn handle_acp(translator: &mut Translator, acp: &Value) -> Handled {
    // 上游存在两套 ACP 形状（见 `normalize_acp`）；先归一成扁平形态再匹配。
    let normalized = normalize_acp(acp);
    let acp = normalized.as_ref().unwrap_or(acp);
    match acp.get("type").and_then(Value::as_str).unwrap_or("") {
        "agent_message_chunk" => {
            let text = chunk_text(acp);
            if text.is_empty() {
                return Handled::default();
            }
            Handled {
                frames: vec![translator.push_content(&text)],
                ..Handled::default()
            }
        }
        "agent_thought_chunk" => {
            let text = chunk_text(acp);
            if text.is_empty() {
                return Handled::default();
            }
            Handled {
                frames: vec![translator.push_reasoning(&text)],
                ..Handled::default()
            }
        }
        "tool_call" => {
            let name = acp
                .get("tool_name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or("unknown")
                .to_string();
            let input = text_field(acp.get("tool_input"));
            logging::verbose(
                "[MonkeyCode]",
                &format!("工具调用 {}（{}）", name, preview(&input, 120)),
            );
            Handled {
                frames: vec![translator.push_tool_call(&name, &input)],
                ..Handled::default()
            }
        }
        "tool_call_update" => {
            let delta = {
                let text = text_field(acp.get("tool_input"));
                if text.is_empty() {
                    text_field(acp.get("delta"))
                } else {
                    text
                }
            };
            let name = acp
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let status = acp.get("status").and_then(Value::as_str).unwrap_or("");
            logging::verbose(
                "[MonkeyCode]",
                &format!(
                    "工具调用更新 status={} name={} delta={}",
                    if status.is_empty() { "-" } else { status },
                    if name.is_empty() { "-" } else { &name },
                    preview(&delta, 120),
                ),
            );
            // 参考只打日志不产帧；文档（06-acp-event-reference）把 tool_input /
            // delta 描述为**增量**，OpenAI 侧按增量下发客户端才能拼出完整参数
            // （与参考 Responses 分支的 function_call_arguments.delta 同口径）。
            match translator.append_tool_arguments(&name, &delta) {
                Some(frame) => Handled {
                    frames: vec![frame],
                    ..Handled::default()
                },
                None => Handled { progress: !status.is_empty(), ..Handled::default() },
            }
        }
        "usage_update" => {
            merge_usage(&mut translator.usage, acp);
            Handled::default()
        }
        "plan" => {
            logging::verbose("[MonkeyCode]", "收到 plan 事件（仅记录）");
            Handled::default()
        }
        "available_commands_update" => {
            logging::verbose("[MonkeyCode]", "收到 available_commands_update（仅记录）");
            Handled::default()
        }
        other => {
            logging::verbose("[MonkeyCode]", &format!("忽略 ACP 事件：{other}"));
            Handled::default()
        }
    }
}

/// ACP 文本块：`text` 优先、`content` 兜底（参考 `acp.text || acp.content`）。
fn chunk_text(acp: &Value) -> String {
    let text = acp
        .get("text")
        .and_then(Value::as_str)
        .or_else(|| acp.get("content").and_then(Value::as_str))
        .unwrap_or("");
    text.to_string()
}

/// 取一个字段的文本形态（字符串原样；其它 JSON 值取序列化文本）。
fn text_field(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// Agent 提问 → `reply-question`（自动回复，参考 `task-runner.ts:214-231`）。
///
/// 载荷由 `decode_data` 解出：取提问对象的 `request_id`（旧形态用 `id`）；
/// 回帧载荷照官方客户端 `approvals.ts::sendAskAnswersVia`：
/// `{request_id, answers_json, cancelled}`（`answers_json: ""` 表示空答案）。
/// 取不到 request_id 时返回 None（不硬造一条对不上号的回复）。
fn auto_reply_question(payload: Option<&Value>) -> Option<String> {
    let payload = payload?;
    let request_id = payload
        .get("request_id")
        .and_then(Value::as_str)
        .or_else(|| payload.get("id").and_then(Value::as_str))?;
    if request_id.trim().is_empty() {
        return None;
    }
    Some(uplink_frame(
        "reply-question",
        &json!({
            "request_id": request_id,
            "answers_json": "",
            "cancelled": false,
        }),
    ))
}

/// `task-error` / `error` 的 data → 可读原因（先 `decode_data`，再按字符串 /
/// 对象取 error/message/msg/detail；都没有时给一句兜底）。
fn error_detail(data: Option<&Value>) -> String {
    // 解不出来就退回原值（可能是一条裸的纯文本错误串），别把原因整个丢掉。
    let decoded = decode_data(data).or_else(|| data.cloned());
    match decoded {
        Some(Value::String(text)) if !text.trim().is_empty() => text.trim().to_string(),
        Some(Value::Object(object)) => ["error", "message", "msg", "detail"]
            .iter()
            .find_map(|key| object.get(*key).and_then(Value::as_str))
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| "任务执行出错（上游未给原因）".to_string()),
        _ => "任务执行出错（上游未给原因）".to_string(),
    }
}

/// `usage_update` / `task-ended.usage` 的计数并入（参考按**累计值**处理：
/// 非零才覆盖，`06-acp-event-reference` 明确「当前代理实现将其作为累计值」）。
fn merge_usage(usage: &mut Usage, source: &Value) {
    for (key, slot) in [
        ("input_tokens", 0usize),
        ("output_tokens", 1),
        ("total_tokens", 2),
    ] {
        let value = source
            .get(key)
            .and_then(Value::as_i64)
            .or_else(|| source.get(key).and_then(Value::as_f64).map(|n| n as i64))
            .unwrap_or(0);
        if value == 0 {
            continue;
        }
        match slot {
            0 => usage.input = value,
            1 => usage.output = value,
            _ => usage.total = value,
        }
    }
}

/// `task-ended` 的 data（base64(JSON)）里的最终 usage（01-task-stream 下行表：
/// `task-ended` 的 data 可能是 `{"usage": {...}}`）。
fn merge_final_usage(translator: &mut Translator, data: Option<&Value>) {
    let Some(payload) = decode_data(data) else {
        return;
    };
    let usage = payload.get("usage").unwrap_or(&payload);
    merge_usage(&mut translator.usage, usage);
}

/// 日志用的截断预览（按字符，避免把中文切成半个字）。
fn preview(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}
