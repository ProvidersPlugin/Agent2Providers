//! CatPaw **轮次判定**：三条规则决定这次请求怎么走上游（架构文档 §9.2）。
//!
//! ── 三规则的顺序是不可交换的（不要「优化」成别的顺序）──────
//! ```text
//!   1. 历史末尾是 assistant(tool_calls) + 全部 tool 结果，且 tool_call_id
//!      能命中注册表 → 工具续接：turn 直接提交那条 tool 消息，不 round；
//!   2. 请求带 x-session-id 且注册表存在映射 → 长会话新轮次：复用
//!      conversationId，round 只提交指纹链定位出的增量；
//!   3. 其余 → 全新会话：新 conversationId，round 提交全量历史。
//! ```
//!
//! 为什么顺序不能换：规则 1 判的是「这一轮是不是上一轮的延续」，规则 2 判的是
//! 「这个客户端会话有没有可复用的 conversationId」。工具续接请求**同样带**
//! x-session-id，若先判规则 2 就会走成「往同一 conversation 再 round 一条 user
//! 消息」—— 而上游此时还在等 tool 结果，round 与 turn 的历史就串了。
//!
//! ── 指纹不匹配为什么是「重建」而不是「报错」─────────────────
//! 客户端压缩/改写历史是**正常行为**（上下文过长时客户端会自行删旧消息）。
//! 那时旧 conversation 无法增量续接，但用户的问题本身没问题 —— 作废映射、
//! 全量重开一轮，用户无感（只是那一轮多传一次历史）。报错会让「客户端做了一件
//! 合理的事」变成用户可见的失败。
//!
//! 同一条原则也管工具续接（规则 1）：客户端把上游返回的**一个** `tool_call`
//! 批次展开成多个子调用、并给子调用 2+ 合成自己的 id（Hermes 的
//! `expand_local_tool_batches` 就是这样），是完全合法的改写 —— 注册表里从来
//! 没有那些 id。此时若按「集合必须严格相等」报 400，这个客户端会话会在 TTL
//! 内**所有**重试上撞同一堵墙。所以集合对不上时同样作废会话、落规则 3 全量重建。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 纯判定：不发网络请求、不持锁、不读盘；零 unwrap/expect/panic。

use serde_json::Value;

use crate::logging;

use super::conversation::{registry, short_id, ConversationRequest};
use super::fingerprint::{locate_increment, ChainPosition};
use super::messages::normalize_continuation_messages;
use super::models::CatPawError;
use super::prepare::Prepared;
use super::registry::{InvalidationReason, SessionRecord, SessionResolution};

/// 新建 conversationId / turnRequestId（原实现用 `randomUUID()`）
fn new_request_id() -> String {
    crate::ids::new_conversation_id()
}
/// 本次请求的执行模式
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum TurnMode {
    /// 规则 1：工具续接（turn 提交 tool 结果，不 round）
    ToolContinuation,
    /// 规则 2：长会话新轮次（复用 conversationId，round 只提交增量）
    SessionRound,
    /// 规则 3：全新会话（新 conversationId，round 全量）
    NewRound,
}

impl TurnMode {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            TurnMode::ToolContinuation => "tool-continuation",
            TurnMode::SessionRound => "session-round",
            TurnMode::NewRound => "new-round",
        }
    }
}

/// 判定结果
pub(super) struct Decision {
    pub(super) mode: TurnMode,
    pub(super) conversation_id: String,
    /// 要提交的 round 消息（`None` = 不 round，即工具续接）
    pub(super) round_messages: Option<Vec<Value>>,
    /// 命中的会话记录（决定收尾时怎么写回注册表）
    pub(super) session: Option<SessionRecord>,
    /// 工具续接时提交的那条 tool 消息
    pub(super) continuation: Option<Value>,
}

/// 轮次判定（原实现 `streamRequest` 开头那一段，**顺序照抄**）
pub(super) fn decide(
    request: &ConversationRequest,
    prepared: &Prepared,
    persistent: bool,
) -> Result<Decision, CatPawError> {
    let raw_messages = request
        .body
        .get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let registry = registry();

    // ── 规则 1：工具续接（按 tool_call_id 命中注册表）──────────────
    // 无状态/并发冲突时**仍然**走这条判定（原实现把 `clientSessionId` 置空只是
    // 关掉「长会话映射」那一读，call 索引那一路照常命中）
    let mut hits: Vec<SessionRecord> = Vec::new();
    for call_id in pending_call_ids(raw_messages) {
        if let Some(record) = registry.lookup_by_call_id(&call_id) {
            if !hits.iter().any(|hit| hit.conversation_id == record.conversation_id) {
                hits.push(record);
            }
        }
    }
    if hits.len() > 1 {
        // 同一批 tool_call_id 命中了两条不同的 conversation：客户端把两轮工具
        // 调用的结果混在一起了，续接到哪一条都是错的
        return Err(CatPawError::bad_request("tool_call_id 命中多个待处理工具会话"));
    }
    if let Some(session) = hits.into_iter().next() {
        match continuation_message(raw_messages, &session) {
            Ok(continuation) => {
                return Ok(Decision {
                    mode: TurnMode::ToolContinuation,
                    conversation_id: session.conversation_id.clone(),
                    round_messages: None,
                    session: Some(session),
                    continuation: Some(continuation),
                });
            }
            Err(Diverged::Mismatch) => {
                // 客户端改写了 tool_call_id（把一批展开成多个子调用、或上游发过
                // 空 id 的 tool_use 块）：注册表里没有那些合成 id，续接无从谈起。
                // 与 sync-mismatch 同一处置 —— 作废映射，交给规则 2/3 重建。
                logging::verbose(
                    "[CatPaw]",
                    &format!(
                        "会话 {} 的 tool_result 集合与待处理 tool_calls 不一致（tool-diverged），作废后重建",
                        short_id(&session.conversation_id),
                    ),
                );
                registry.invalidate_by_conversation(
                    &session.conversation_id,
                    InvalidationReason::SyncMismatch,
                );
            }
            Err(Diverged::Malformed(error)) => return Err(error),
        }
    }

    // ── 规则 2：长会话新轮次（x-session-id 命中注册表）────────────
    if persistent {
        match registry.resolve(
            &request.session_id,
            prepared.resolution.model_type,
            &request.account_id,
        ) {
            SessionResolution::Reuse(session) => {
                // 指纹链定位增量：末条已同步指纹在本次历史里的位置之后即为增量
                match locate_increment(&prepared.messages, &session.fingerprints) {
                    ChainPosition::Incremental { start } => {
                        let mut increment = prepared.messages[start..].to_vec();
                        if increment.is_empty() {
                            // 客户端没带新消息（重发同一轮）：只提交末尾那条
                            increment.push(prepared.final_message.clone());
                        }
                        return Ok(Decision {
                            mode: TurnMode::SessionRound,
                            conversation_id: session.conversation_id.clone(),
                            round_messages: Some(increment),
                            session: Some(session),
                            continuation: None,
                        });
                    }
                    ChainPosition::Mismatch => {
                        // 客户端压缩/改写了历史：旧 conversation 无法增量续接，
                        // 作废后按规则 3 全量重建（原实现 `sync-mismatch` 路径）
                        logging::verbose(
                            "[CatPaw]",
                            &format!(
                                "会话 {} 指纹不匹配（sync-mismatch），作废后全量重建",
                                short_id(&session.conversation_id),
                            ),
                        );
                        registry.invalidate(&request.session_id, InvalidationReason::SyncMismatch);
                    }
                }
            }
            SessionResolution::Rebuild { reason } => {
                if let Some(reason) = reason {
                    // 过期 / 换模型 / 换账号：注册表已经作废了旧记录，这里只记一笔
                    logging::verbose("[CatPaw]", &format!("会话重建 reason={}", reason.as_str()));
                }
            }
        }
    }

    // ── 规则 3：全新会话（全量 round）────────────────────────────
    Ok(Decision {
        mode: TurnMode::NewRound,
        conversation_id: new_request_id(),
        round_messages: Some(prepared.messages.clone()),
        session: None,
        continuation: None,
    })
}

/// 历史末尾那段「待响应的 assistant tool_call」的起点下标
/// （原实现 `findPendingToolCallStart`）。
///
/// 从后往前走，找最近一条「带非空 tool_calls 的 assistant」，再要求它之后的消息
/// **全是 tool 且把每个 tool_call_id 都覆盖到**：
///   - 覆盖完整 → 这就是待响应集合（返回它的下标）；
///   - 全是 tool 但没覆盖完 → 本轮不是工具续接（原实现 `return -1`）；
///   - 中间混了别的角色 → 继续往前找（原实现内层 break 后继续外层循环）。
fn pending_tool_call_start(messages: &[Value]) -> Option<usize> {
    let mut index = messages.len();
    while index > 0 {
        index -= 1;
        let message = &messages[index];
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(calls) = message.get("tool_calls").and_then(Value::as_array) else {
            continue;
        };
        if calls.is_empty() {
            continue;
        }
        let mut remaining: Vec<String> = calls
            .iter()
            .filter_map(|call| call.get("id").and_then(Value::as_str))
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect();
        let mut valid = true;
        for item in &messages[index + 1..] {
            if item.get("role").and_then(Value::as_str) != Some("tool") {
                valid = false;
                break;
            }
            let call_id = item
                .get("tool_call_id")
                .or_else(|| item.get("toolCallId"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            remaining.retain(|id| id != call_id);
        }
        if !valid {
            continue;
        }
        return if remaining.is_empty() { Some(index) } else { None };
    }
    None
}

/// 历史末尾待响应工具调用的 id 列表（原实现 `pendingCallIds`）
pub(super) fn pending_call_ids(messages: &[Value]) -> Vec<String> {
    let Some(start) = pending_tool_call_start(messages) else {
        return Vec::new();
    };
    messages[start]
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .filter_map(|call| call.get("id").and_then(Value::as_str))
                .filter(|id| !id.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// 工具续接要提交的那条 tool 消息（原实现 `continuationToolMessage`）。
///
/// ── 与原实现的一处差异（见模块头第 1 条）─────────────────────
/// 原实现用会话里存的 `toolCalls` 回填 `toolName`，这里改成从**本次请求历史**
/// 里那条 assistant 消息的 `tool_calls` 取 —— 两者是同一批数据（会话里那份就是
/// 上一轮上游返回、被客户端回显的），而本次请求的值不会过期。
/// 回填是必需的：toolName 进指纹（`fingerprint.rs`），缺了会让客户端下一轮回显
/// 的 tool 消息与注册表指纹对不上，误判「历史被改写」而全量重建。
///
/// 出参区分「客户端改写了历史」（[`Diverged::Mismatch`]，调用方作废重建）与
/// 「请求本身不合法」（[`Diverged::Malformed`]，照旧 400）。
fn continuation_message(
    raw_messages: &[Value],
    session: &SessionRecord,
) -> Result<Value, Diverged> {
    let Some(start) = pending_tool_call_start(raw_messages) else {
        return Err(Diverged::Malformed(CatPawError::bad_request(
            "工具结果续接请求缺少对应的 assistant tool_calls",
        )));
    };
    let names: Vec<(String, String)> = raw_messages[start]
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .map(|call| {
                    (
                        call.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
                        call.pointer("/function/name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let mut continuation = normalize_continuation_messages(&raw_messages[start + 1..])
        .map_err(|error| Diverged::Malformed(error.into()))?;
    let is_single_tool = continuation.len() == 1
        && continuation
            .first()
            .and_then(|message| message.get("type"))
            .and_then(Value::as_str)
            == Some("tool");
    if !is_single_tool {
        return Err(Diverged::Malformed(CatPawError::bad_request(
            "工具结果续接请求只能包含当前 tool_call 对应的 tool 结果",
        )));
    }
    let message = continuation.remove(0);
    let blocks = message
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut seen: Vec<String> = Vec::new();
    let mut rewritten: Vec<Value> = Vec::with_capacity(blocks.len());
    for block in blocks {
        let mut block = block;
        if block.get("type").and_then(Value::as_str) == Some("tool_result") {
            let call_id = block
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            seen.push(call_id.clone());
            if let Some(name) = names
                .iter()
                .find(|(id, _)| *id == call_id)
                .map(|(_, name)| name)
                .filter(|name| !name.is_empty())
            {
                if let Some(object) = block.as_object_mut() {
                    object.insert("toolName".to_string(), Value::String(name.clone()));
                }
            }
        }
        rewritten.push(block);
    }
    // 待响应集合必须与实际提交的 tool_result 一致。
    //
    // 不一致**不是客户端错误**：客户端把上游返回的一个 tool_call 批次展开成多个
    // 子调用时，子调用 2+ 的 id 是它自己确定性合成的（注册表里从来没有过）；
    // 上游发过空 toolCallId 的 tool_use 块时，注册集合也会小于客户端可见集合。
    // 两种都是合法的历史改写 —— 交回调用方作废会话、走全量重建。
    if session.pending_call_ids.len() != seen.len()
        || session.pending_call_ids.iter().any(|id| !seen.contains(id))
    {
        return Err(Diverged::Mismatch);
    }
    let Some(mut object) = message.as_object().cloned() else {
        return Err(Diverged::Malformed(CatPawError::bad_request(
            "工具结果续接请求缺少 tool 消息",
        )));
    };
    object.insert("content".to_string(), Value::Array(rewritten));
    Ok(Value::Object(object))
}

/// 工具续接取不出可提交消息的两种原因（决定调用方是重建还是报错）。
///
/// 分开的理由：`Mismatch` 是「客户端做了一件合理的事」（改写 tool_call_id），
/// 用户的问题本身没问题，作废会话重建即可；`Malformed` 是请求真的不合规
/// （tool 消息结构不对、角色顺序错乱），照旧 400 让客户端看到原因。
enum Diverged {
    Mismatch,
    Malformed(CatPawError),
}

/// 本轮「实际提交给上游」的消息指纹（写回指纹链用）。
///
/// 长会话只提交增量、工具续接只提交那条 tool 消息 —— 必须与实际发出去的一致，
/// 否则下一次  会在客户端历史里定位到错误的位置。
///
/// `synthetic_tail` = 末尾那条 user 是本地补的（客户端历史以 tool 结尾）：
/// 它**不参与指纹**。指纹链的用途是在客户端历史里定位增量，而这条消息压根不在
/// 客户端历史里 —— 写进链会让下一轮 `locate_increment` 找不到它、判成
/// 「历史被改写」，于是每轮都全量重建。
pub(super) fn submitted_fingerprints(decision: &Decision, synthetic_tail: bool) -> Vec<String> {
    let mut submitted = match &decision.round_messages {
        Some(messages) if synthetic_tail => {
            super::fingerprint::fingerprints_for(&messages[..messages.len().saturating_sub(1)])
        }
        Some(messages) => super::fingerprint::fingerprints_for(messages),
        None => Vec::new(),
    };
    if let Some(continuation) = &decision.continuation {
        submitted.push(super::fingerprint::message_fingerprint(continuation));
    }
    submitted
}
