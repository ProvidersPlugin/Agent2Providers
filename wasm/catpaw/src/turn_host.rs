//! CatPaw 已确认的固定五步 ABI 消费者。只有宿主私有 Owner 能证明归属。
//! handle/receipt 都是可复制 JSON 字符串：本模块的格式校验不授予发送或清理权限。
//! 无普通 HTTP 回退，无 Drop 请求，无重试；主程序未登记能力时明确失败。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::collections::BTreeSet;

use crate::{errors, prepare, stream::ParsedTurn};

pub(super) const PROFILE: &str = "catpaw.turn.v1";
pub(super) const CAPABILITY: &str = "http.catpaw-turn.v1";
pub(super) const MAX_MATERIAL: usize = 16 * 1024 * 1024;
const MAX_CHUNK: usize = 64 * 1024;
const MAX_READS: u64 = 16_384;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    NewRound,
    ToolContinuation,
}

pub(super) struct PreparedCall {
    handle: String,
    pub(super) mode: Mode,
    pub(super) conversation_id: String,
    pub(super) turn_request_id: String,
    pub(super) message_ids: Vec<String>,
    pub(super) response_id: String,
    pub(super) created: u64,
    pub(super) session_revision: u64,
    pub(super) previous: Option<Value>,
    start_attempted: bool,
    close_attempted: bool,
}

/// 仅由本次 start 成功回包解析产生。无 Clone/Deserialize/公开字段构造入口。
/// 真正真实性仍由宿主逐次核验原 Owner，不能从此 Rust 类型推断远端事实。
pub(super) struct Active {
    receipt: String,
    sequence: u64,
    eof: bool,
    settle_attempted: bool,
}

pub(super) struct ReadChunk {
    pub(super) bytes: Vec<u8>,
    pub(super) eof: bool,
}

pub(super) struct Settlement {
    pub(super) session_revision: u64,
    pub(super) awaiting_tools: bool,
}

pub(super) enum Cleanup {
    NotNeeded,
    Confirmed,
    Unconfirmed,
}

impl PreparedCall {
    /// constraints 是本次本地校验的请求约束；不含账号、会话键、URL 或远端 ID。
    pub(super) fn open(constraints: &Value, message_count: usize) -> Result<Self> {
        if message_count == 0 {
            return Err(errors::invalid("CatPaw 本次消息数量不能为 0"));
        }
        bounded(constraints, 3 * 1024 * 1024)?;
        let reply = invoke(
            "catpaw.turn.prepare",
            json!({
                "constraints": constraints, "message_count": message_count
            }),
        )?;
        // 只能从本次宿主回包提取候选句柄；坏回包也不能用请求里的任何 ID 代替。
        let cleanup_handle = reply
            .get("handle")
            .and_then(Value::as_str)
            .filter(|text| valid_token(text))
            .map(str::to_string);
        let parsed = Self::parse(reply, message_count);
        if parsed.is_err() {
            if let Some(handle) = cleanup_handle {
                // 宿主 close 必须复核当前 Owner；不存在/不属于本次则不发远端请求。
                let _ = invoke("catpaw.turn.close", json!({"handle": handle}));
            }
        }
        parsed
    }

    fn parse(reply: Value, message_count: usize) -> Result<Self> {
        exact(
            &reply,
            &[
                "profile",
                "handle",
                "mode",
                "conversation_id",
                "turn_request_id",
                "message_ids",
                "response_id",
                "created",
                "session_revision",
                "previous",
            ],
        )?;
        profile(&reply)?;
        let handle = token(&reply, "handle")?.to_string();
        let mode = match reply["mode"].as_str() {
            Some("new_round") => Mode::NewRound,
            Some("tool_continuation") => Mode::ToolContinuation,
            _ => {
                return Err(protocol(
                    "CatPaw 宿主返回了本片未支持的轮次模式，未自动重建",
                ))
            }
        };
        let conversation_id = correlation(&reply, "conversation_id")?.to_string();
        let turn_request_id = correlation(&reply, "turn_request_id")?.to_string();
        if conversation_id == turn_request_id {
            return Err(protocol("CatPaw 宿主关联 ID 发生冲突"));
        }
        let ids = reply["message_ids"]
            .as_array()
            .filter(|items| items.len() == message_count)
            .ok_or_else(|| protocol("CatPaw 宿主消息 ID 数量与本次请求不符"))?;
        let mut message_ids = Vec::with_capacity(ids.len());
        let mut seen = BTreeSet::new();
        for id in ids {
            let id = id
                .as_str()
                .filter(|text| valid_token(text))
                .ok_or_else(|| protocol("CatPaw 宿主消息关联 ID 格式无效"))?;
            if id == conversation_id || id == turn_request_id || !seen.insert(id) {
                return Err(protocol("CatPaw 宿主消息关联 ID 重复或与轮次冲突"));
            }
            message_ids.push(id.to_string());
        }
        let response_id = token(&reply, "response_id")?.to_string();
        let created = reply["created"]
            .as_u64()
            .filter(|time| *time > 0 && *time <= 253_402_300_799)
            .ok_or_else(|| protocol("CatPaw 宿主未提供有效响应时间，未补零"))?;
        let session_revision = reply["session_revision"]
            .as_u64()
            .ok_or_else(|| protocol("CatPaw 宿主会话版本格式无效"))?;
        let previous = match mode {
            Mode::NewRound if session_revision == 0 && reply["previous"].is_null() => None,
            Mode::ToolContinuation if session_revision > 0 && reply["previous"]["conversation_id"] == conversation_id => {
                exact(&reply["previous"], &["conversation_id"])?;
                Some(reply["previous"].clone())
            }
            _ => {
                return Err(protocol(
                    "CatPaw 新轮次不接受旧会话快照或旧版本，未自动重建",
                ))
            }
        };
        Ok(Self {
            handle,
            mode,
            conversation_id,
            turn_request_id,
            message_ids,
            response_id,
            created,
            session_revision,
            previous,
            start_attempted: false,
            close_attempted: false,
        })
    }

    /// body 内嵌 JSON 保留为文本，宿主必须严格解析并拒绝重复键，不直接 from_value 接受。
    pub(super) fn start(
        &mut self,
        round_json: Option<String>,
        turn_json: String,
        history_json: String,
        request_context_json: String,
    ) -> Result<Active> {
        if self.start_attempted || self.close_attempted {
            return Err(protocol("CatPaw 本次轮次已提交或关闭，未重复发送"));
        }
        if (self.mode == Mode::NewRound) != round_json.is_some() {
            return Err(protocol("CatPaw round 材料与宿主已分配模式不一致"));
        }
        let params = json!({"handle": self.handle, "round_json": round_json,
            "turn_json": turn_json, "history_json": history_json,
            "request_context_json": request_context_json, "session_revision": self.session_revision});
        bounded(&params, MAX_MATERIAL)?;
        // 未知结果也永久占用本对象的一次发送机会；不是把 start 错误当作未发送。
        self.start_attempted = true;
        let reply = invoke("catpaw.turn.start", params)?;
        exact(
            &reply,
            &[
                "profile",
                "handle",
                "receipt",
                "conversation_id",
                "turn_request_id",
                "round_accepted",
                "turn_opened",
                "status",
                "content_type",
            ],
        )?;
        self.matches(&reply)?;
        if reply["conversation_id"].as_str() != Some(self.conversation_id.as_str())
            || reply["turn_request_id"].as_str() != Some(self.turn_request_id.as_str())
            || reply["round_accepted"].as_bool() != Some(self.mode == Mode::NewRound)
            || reply["turn_opened"].as_bool() != Some(true)
            || reply["status"].as_u64() != Some(200)
            || reply["content_type"].as_str() != Some("text/event-stream")
        {
            return Err(protocol(
                "CatPaw 宿主未确认本次真实 round/turn 响应，未开始读取",
            ));
        }
        let receipt = token(&reply, "receipt")?.to_string();
        Ok(Active {
            receipt,
            sequence: 0,
            eof: false,
            settle_attempted: false,
        })
    }

    pub(super) fn read(&self, active: &mut Active) -> Result<ReadChunk> {
        if self.close_attempted
            || !self.start_attempted
            || active.eof
            || active.settle_attempted
            || active.sequence >= MAX_READS
        {
            return Err(protocol("CatPaw 本次读取已结束或超过 16384 次限制"));
        }
        let sequence = active.sequence;
        // 在跨 ABI 前消费序号，读取失败不复用同一次机会。
        active.sequence += 1;
        let reply = invoke(
            "catpaw.turn.read",
            json!({"handle": self.handle,
            "receipt": active.receipt, "sequence": sequence}),
        )?;
        exact(
            &reply,
            &["profile", "handle", "receipt", "sequence", "data", "eof"],
        )?;
        self.matches_active(&reply, active)?;
        if reply["sequence"].as_u64() != Some(sequence) {
            return Err(protocol("CatPaw 宿主流分片序号不一致，未拼接乱序材料"));
        }
        let encoded = reply["data"]
            .as_str()
            .filter(|data| data.len() <= 87_384)
            .ok_or_else(|| protocol("CatPaw 宿主流分片格式无效或超过限制"))?;
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| protocol("CatPaw 宿主流分片不是规范 Base64"))?;
        if bytes.len() > MAX_CHUNK || STANDARD.encode(&bytes) != encoded {
            return Err(protocol("CatPaw 宿主流分片过大或编码不规范"));
        }
        let eof = reply["eof"]
            .as_bool()
            .ok_or_else(|| protocol("CatPaw 宿主未确认读取是否正常结束"))?;
        if bytes.is_empty() && !eof {
            return Err(protocol("CatPaw 宿主返回无进展读取，未后台轮询"));
        }
        active.eof = eof;
        Ok(ReadChunk { bytes, eof })
    }

    pub(super) fn settle(&self, active: &mut Active, parsed: &ParsedTurn) -> Result<Settlement> {
        if self.close_attempted || !active.eof || active.settle_attempted {
            return Err(protocol("CatPaw 尚无真实 EOF 或已提交结算材料"));
        }
        let parsed_json = encode(&json!({"upstream_message": parsed.upstream_message,
            "observed_conversation_id": parsed.observed_conversation_id,
            "done_event_name": parsed.done_event_name, "usage": parsed.usage}))?;
        let read_sequence = active
            .sequence
            .checked_sub(1)
            .ok_or_else(|| protocol("CatPaw 没有本次真实读取序号"))?;
        active.settle_attempted = true;
        let reply = invoke(
            "catpaw.turn.settle",
            json!({"handle": self.handle,
            "receipt": active.receipt, "session_revision": self.session_revision,
            "read_sequence": read_sequence, "parsed_json": parsed_json}),
        )?;
        exact(
            &reply,
            &[
                "profile",
                "handle",
                "receipt",
                "disposition",
                "state_committed",
                "session_revision",
            ],
        )?;
        self.matches_active(&reply, active)?;
        let awaiting_tools = parsed.finish_reason == "tool_calls";
        let expected = if awaiting_tools {
            "awaiting_tools"
        } else {
            "completed"
        };
        let revision = reply["session_revision"]
            .as_u64()
            .filter(|rev| *rev > self.session_revision)
            .ok_or_else(|| protocol("CatPaw 宿主未确认同源会话新版本"))?;
        if reply["disposition"].as_str() != Some(expected)
            || reply["state_committed"].as_bool() != Some(true)
        {
            return Err(protocol("CatPaw 宿主收尾或工具交接未确认，未宣布成功"));
        }
        Ok(Settlement {
            session_revision: revision,
            awaiting_tools,
        })
    }

    /// close 只归还本次宿主句柄；不能传 conversationId、turnId 或自行选择远端状态。
    /// 不在 Drop 调用：WASM trap 时析构没有保证，Owner 必须独立保留真实收尾责任。
    pub(super) fn close(&mut self) -> Result<Cleanup> {
        if self.close_attempted {
            return Err(protocol("CatPaw 本次句柄已关闭，未重复发送清理"));
        }
        self.close_attempted = true;
        let reply = invoke("catpaw.turn.close", json!({"handle": self.handle}))?;
        exact(&reply, &["profile", "handle", "closed", "cleanup"])?;
        self.matches(&reply)?;
        if reply["closed"].as_bool() != Some(true) {
            return Err(protocol("CatPaw 宿主句柄关闭未确认"));
        }
        match reply["cleanup"].as_str() {
            Some("not_needed") => Ok(Cleanup::NotNeeded),
            Some("confirmed") => Ok(Cleanup::Confirmed),
            Some("unconfirmed") => Ok(Cleanup::Unconfirmed),
            _ => Err(protocol("CatPaw 宿主清理结果未知，未猜测已停止")),
        }
    }

    fn matches(&self, value: &Value) -> Result<()> {
        profile(value)?;
        if value["handle"].as_str() != Some(self.handle.as_str()) {
            return Err(protocol("CatPaw 宿主回包不属于本次句柄"));
        }
        Ok(())
    }

    fn matches_active(&self, value: &Value, active: &Active) -> Result<()> {
        self.matches(value)?;
        if value["receipt"].as_str() != Some(active.receipt.as_str()) {
            return Err(protocol("CatPaw 宿主回包与本次发送记录不一致"));
        }
        Ok(())
    }
}

fn invoke(method: &str, params: Value) -> Result<Value> {
    bounded(&params, MAX_MATERIAL)?;
    let reply = sdk::host(method, params).map_err(errors::turn)?;
    bounded(&reply, MAX_MATERIAL)?;
    Ok(reply)
}

pub(super) fn exact(value: &Value, fields: &[&str]) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| protocol("CatPaw 受控会话材料必须是对象"))?;
    if object.len() != fields.len() || fields.iter().any(|key| !object.contains_key(*key)) {
        return Err(protocol("CatPaw 受控会话材料含缺失或未知字段"));
    }
    Ok(())
}

pub(super) fn bounded(value: &Value, limit: usize) -> Result<()> {
    crate::stream_snapshot::nodes(value)?;
    prepare::json_bytes(value, limit)
        .map(|_| ())
        .map_err(|_| protocol("CatPaw 受控会话材料超过大小限制"))
}

pub(super) fn encode(value: &Value) -> Result<String> {
    bounded(value, MAX_MATERIAL)?;
    serde_json::to_string(value).map_err(|_| protocol("CatPaw 受控会话材料无法编码"))
}

fn profile(value: &Value) -> Result<()> {
    if value["profile"].as_str() == Some(PROFILE) {
        Ok(())
    } else {
        Err(protocol("CatPaw 宿主受控会话 profile 不匹配"))
    }
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
}

fn token<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .filter(|text| valid_token(text))
        .ok_or_else(|| protocol("CatPaw 宿主句柄或关联 ID 格式无效"))
}

fn correlation<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    token(value, field)
}
fn protocol(message: &str) -> Error {
    errors::protocol(message)
}
