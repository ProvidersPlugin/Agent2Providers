//! 签名仅按同账号、同模型、同调用方及精确调用 ID 回填；不凭工具名称猜测。
//! Claude 的完整思考部件来自本次真实响应，保留原字节内容，不插入合成思考哨兵。

use super::{credentials, util};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const SENTINEL: &str = "skip_thought_signature_validator";
const KEY: &str = "thought-signatures";
const TTL: i64 = 86_400_000;
const LIMIT: usize = 220 * 1024;

pub fn call_signature(call: &Value) -> Option<String> {
    [
        "/extra_content/google/thought_signature",
        "/thought_signature",
        "/thoughtSignature",
    ]
    .iter()
    .find_map(|pointer| {
        call.pointer(pointer)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

pub fn synthetic(signature: &str) -> bool {
    matches!(
        signature.trim(),
        SENTINEL
            | "context_engineering_is_the_way_to_go"
            | "c2tpcF90aG91Z2h0X3NpZ25hdHVyZV92YWxpZGF0b3I="
    )
}

fn missing(value: &Value) -> bool {
    value
        .as_str()
        .is_none_or(|signature| signature.trim().is_empty() || synthetic(signature))
}

pub struct Cache {
    conversation: String,
    binding: String,
    capture_thought: bool,
    correlated: bool,
    items: BTreeMap<String, String>,
    turns: BTreeMap<String, Vec<Value>>,
    pending_parts: Vec<usize>,
    pending_thought: Option<Value>,
    response_parts: Vec<Value>,
    response_calls: BTreeSet<String>,
    response_has_tools: bool,
    over_capacity: bool,
}

impl Cache {
    pub fn load(
        session: &str,
        caller_session: &str,
        caller: &str,
        model: &str,
        secret: &Value,
    ) -> Result<Self> {
        let correlated = caller.len() == 64 && caller.bytes().all(|byte| byte.is_ascii_hexdigit());
        if !caller.is_empty() && !correlated {
            return Err(
                Error::http(400, "Antigravity 调用方关联摘要无效").with_code("local_precondition")
            );
        }
        // 缺宿主调用方时只使用本次随机命名空间，不查找或延续其它调用方的思考历史。
        let caller = if correlated {
            caller.to_string()
        } else {
            util::random_hex(32)?
        };
        let binding = credentials::binding(secret);
        let conversation = format!(
            "gemini-{}",
            util::digest(
                json!([binding, caller, caller_session, session])
                    .to_string()
                    .as_bytes()
            )
        );
        let (items, turns) = if correlated {
            let old = sdk::state::conversation_get(&conversation, KEY, 1)?;
            if !old.compatible {
                return Err(Error::http(
                    409,
                    "Antigravity 签名状态格式已变化，请新建会话",
                ));
            }
            if old.value["binding"] == binding {
                (
                    serde_json::from_value(old.value["items"].clone()).unwrap_or_default(),
                    serde_json::from_value(old.value["turns"].clone()).unwrap_or_default(),
                )
            } else {
                (BTreeMap::new(), BTreeMap::new())
            }
        } else {
            (BTreeMap::new(), BTreeMap::new())
        };
        Ok(Self {
            conversation,
            binding,
            capture_thought: correlated && model.to_ascii_lowercase().starts_with("claude-"),
            correlated,
            items,
            turns,
            pending_parts: Vec::new(),
            pending_thought: None,
            response_parts: Vec::new(),
            response_calls: BTreeSet::new(),
            response_has_tools: false,
            over_capacity: false,
        })
    }

    pub fn session_key(&self) -> &str {
        &self.conversation
    }

    pub fn fill(&self, envelope: &mut Value) {
        let model = envelope["model"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase();
        let claude = model.starts_with("claude-");
        let fallback = model.starts_with("gemini-") || model.starts_with("gpt-oss-");
        let Some(contents) = envelope
            .pointer_mut("/request/contents")
            .and_then(Value::as_array_mut)
        else {
            return;
        };
        for turn in contents.iter_mut().filter(|turn| turn["role"] == "model") {
            let Some(parts) = turn["parts"].as_array_mut() else {
                continue;
            };
            if claude {
                if let Some(original) = batch_key(parts)
                    .and_then(|key| self.turns.get(&key))
                    .filter(|original| {
                        same_calls(parts, original) && visible_text(parts) == visible_text(original)
                    })
                {
                    // 仅本批精确ID、原名称/参数及可见正文全部相同时，恢复原完整部件和顺序。
                    *parts = original.clone();
                    continue;
                }
            }
            for part in parts {
                if !part["functionCall"].is_object() || !missing(&part["thoughtSignature"]) {
                    continue;
                }
                let id = part["functionCall"]["id"].as_str().unwrap_or("");
                if let Some(signature) = self.items.get(id) {
                    part["thoughtSignature"] = json!(signature);
                } else if fallback {
                    part["thoughtSignature"] = json!(SENTINEL);
                }
            }
        }
    }

    pub fn observe(&mut self, event: &Value) -> Result<()> {
        self.pending_parts.clear();
        if self.capture_thought && !self.over_capacity {
            for candidate in super::errors::inner(event)["candidates"]
                .as_array()
                .into_iter()
                .flatten()
            {
                for part in candidate["content"]["parts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    if part["thought"] == true {
                        self.thought(part)?;
                    } else {
                        self.flush_thought();
                        if part["functionCall"].is_object() {
                            self.response_has_tools = true;
                        }
                        let mut part = part.clone();
                        if part["functionCall"].is_object()
                            && part["functionCall"].get("args").is_none_or(Value::is_null)
                        {
                            part["functionCall"]["args"] = json!({});
                        }
                        self.response_parts.push(part);
                    }
                    if part["functionCall"].is_object() {
                        self.pending_parts
                            .push(self.response_parts.len().saturating_sub(1));
                    }
                }
            }
        }
        if self
            .response_parts
            .iter()
            .map(|part| part.to_string().len())
            .sum::<usize>()
            .saturating_add(
                self.pending_thought
                    .as_ref()
                    .map(|part| part.to_string().len())
                    .unwrap_or(0),
            )
            > LIMIT
        {
            self.over_capacity = true;
            self.response_parts.clear();
            self.pending_thought = None;
            self.pending_parts.clear();
        }
        Ok(())
    }

    fn thought(&mut self, part: &Value) -> Result<()> {
        if self.pending_thought.is_none() {
            self.pending_thought = Some(part.clone());
            return Ok(());
        }
        let previous = self
            .pending_thought
            .as_mut()
            .ok_or_else(|| Error::new("思考部件状态缺失"))?;
        if call_signature(previous).is_some() {
            self.flush_thought();
            self.pending_thought = Some(part.clone());
            return Ok(());
        }
        if let Some(text) = part["text"].as_str() {
            let mut combined = previous["text"].as_str().unwrap_or("").to_string();
            combined.push_str(text);
            previous["text"] = json!(combined);
        }
        if let Some(signature) = call_signature(part).filter(|signature| !synthetic(signature)) {
            previous["thoughtSignature"] = json!(signature);
        }
        Ok(())
    }

    fn flush_thought(&mut self) {
        if let Some(part) = self.pending_thought.take() {
            self.response_parts.push(part);
        }
    }

    pub fn remember(&mut self, frames: &[u8]) -> Result<()> {
        let text = std::str::from_utf8(frames)
            .map_err(|_| Error::http(502, "协议转换结果不是有效文本"))?;
        let mut added = BTreeMap::<String, String>::new();
        let mut ids = Vec::new();
        for line in text.lines().filter_map(|line| line.strip_prefix("data:")) {
            let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            for choice in value["choices"].as_array().into_iter().flatten() {
                for call in choice["delta"]["tool_calls"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    let Some(id) = call["id"].as_str().filter(|id| !id.is_empty()) else {
                        continue;
                    };
                    ids.push(id.to_string());
                    if !self.response_calls.insert(id.to_string()) {
                        return Err(Error::http(
                            502,
                            "Antigravity 同一响应重复了工具调用标识，未混合结果",
                        ));
                    }
                    if let Some(signature) =
                        call_signature(call).filter(|signature| !synthetic(signature))
                    {
                        if added.get(id).is_some_and(|old| old != &signature) {
                            return Err(Error::http(
                                409,
                                "Antigravity 同一响应内的工具调用签名冲突",
                            ));
                        }
                        added.insert(id.to_string(), signature);
                    }
                }
            }
        }
        if self.capture_thought && ids.len() == self.pending_parts.len() {
            for (id, index) in ids.iter().zip(&self.pending_parts) {
                if let Some(part) = self.response_parts.get_mut(*index) {
                    part["functionCall"]["id"] = json!(id);
                }
            }
        }
        self.pending_parts.clear();
        if !added.is_empty() {
            self.persist(&added, &BTreeMap::new())?;
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        self.flush_thought();
        if !self.capture_thought || self.response_calls.is_empty() {
            return Ok(());
        }
        if self.over_capacity {
            return Err(Error::http(
                413,
                "Antigravity 本次工具思考超过缓存容量，请保留原签名或新建会话",
            )
            .with_code("plugin_resource_limit"));
        }
        if !self.response_has_tools {
            return Ok(());
        }
        let signature_present = self
            .response_parts
            .iter()
            .any(|part| call_signature(part).is_some_and(|signature| !synthetic(&signature)));
        if !signature_present {
            return Ok(());
        }
        // 思考文本和签名锚点共同保留；签名在后续工具部件上的合法响应也不能丢掉前面的思考。
        let raw_ids: BTreeSet<String> = self
            .response_parts
            .iter()
            .filter_map(|part| part["functionCall"]["id"].as_str().map(str::to_string))
            .collect();
        if raw_ids != self.response_calls {
            return Err(Error::http(
                502,
                "Antigravity 工具响应与转换后的调用标识不完整对应",
            ));
        }
        let key = batch_key(&self.response_parts)
            .ok_or_else(|| Error::http(502, "Antigravity 工具响应缺少稳定调用标识"))?;
        let mut turns = BTreeMap::new();
        turns.insert(key, self.response_parts.clone());
        self.persist(&BTreeMap::new(), &turns)
    }

    fn capacity(
        &self,
        items: &BTreeMap<String, String>,
        turns: &BTreeMap<String, Vec<Value>>,
    ) -> Result<()> {
        if items.len() > 2048
            || turns.len() > 2048
            || json!({ "binding": self.binding, "items": items, "turns": turns })
                .to_string()
                .len()
                > LIMIT
        {
            return Err(Error::http(
                413,
                "Antigravity 当前会话的真实思考或工具签名超过缓存容量，请新建会话",
            )
            .with_code("plugin_resource_limit"));
        }
        Ok(())
    }

    fn persist(
        &mut self,
        added: &BTreeMap<String, String>,
        new_turns: &BTreeMap<String, Vec<Value>>,
    ) -> Result<()> {
        if !self.correlated {
            return Ok(());
        }
        for _ in 0..4 {
            let previous = sdk::state::conversation_get(&self.conversation, KEY, 1)?;
            if !previous.compatible {
                return Err(Error::http(409, "Antigravity 签名缓存格式冲突"));
            }
            let (mut items, mut turns): (BTreeMap<String, String>, BTreeMap<String, Vec<Value>>) =
                if previous.value["binding"] == self.binding {
                    (
                        serde_json::from_value(previous.value["items"].clone()).unwrap_or_default(),
                        serde_json::from_value(previous.value["turns"].clone()).unwrap_or_default(),
                    )
                } else {
                    (BTreeMap::new(), BTreeMap::new())
                };
            for (id, signature) in added {
                if items.get(id).is_some_and(|old| old != signature) {
                    return Err(Error::http(
                        409,
                        "Antigravity 同一工具调用返回了冲突签名，未替换既有签名",
                    ));
                }
                items.insert(id.clone(), signature.clone());
            }
            for (id, parts) in new_turns {
                if turns.get(id).is_some_and(|old| old != parts) {
                    return Err(Error::http(
                        409,
                        "Antigravity 同一工具调用对应不同思考历史，未覆盖",
                    ));
                }
                turns.insert(id.clone(), parts.clone());
            }
            self.capacity(&items, &turns)?;
            let value = json!({ "binding": self.binding, "items": items, "turns": turns });
            if sdk::state::conversation_compare_exchange(
                &self.conversation,
                KEY,
                1,
                previous.revision,
                value,
                TTL,
            )? {
                self.items = items;
                self.turns = turns;
                return Ok(());
            }
        }
        Err(Error::http(
            409,
            "Antigravity 签名缓存仍被并发更新，未覆盖其他响应",
        ))
    }
}

fn batch_key(parts: &[Value]) -> Option<String> {
    let calls: Vec<&Value> = parts
        .iter()
        .filter(|part| part["functionCall"].is_object())
        .collect();
    if calls.is_empty()
        || calls.iter().any(|part| {
            part["functionCall"]["id"]
                .as_str()
                .is_none_or(str::is_empty)
        })
    {
        return None;
    }
    let ids: Vec<&str> = calls
        .iter()
        .filter_map(|part| part["functionCall"]["id"].as_str())
        .collect();
    Some(util::digest(json!(ids).to_string().as_bytes()))
}

fn visible_text(parts: &[Value]) -> String {
    let mut text = String::new();
    for part in parts.iter().filter(|part| part["thought"] != true) {
        if let Some(value) = part["text"].as_str() {
            text.push_str(value);
        }
    }
    text
}

fn same_calls(left: &[Value], right: &[Value]) -> bool {
    let calls = |parts: &[Value]| {
        parts
            .iter()
            .filter(|part| part["functionCall"].is_object())
            .map(|part| part["functionCall"].clone())
            .collect::<Vec<_>>()
    };
    let calls_left = calls(left);
    !calls_left.is_empty() && calls_left == calls(right)
}
