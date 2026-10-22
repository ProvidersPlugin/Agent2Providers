//! 原生（上游服务端执行）工具声明的**降级**（只保留出站方向要用的部分）。
//!
//! ── 为什么插件只需要「降级」这一半 ──────────────────────────
//! 网关那份 `native_tool.rs` 服务两个方向：
//!   · **入站**（`carry` / `restore`）：把 Responses / Anthropic 的声明保真带到
//!     枢纽形态（Chat），再由出口按目标协议恢复。这一半的前提是「网关同时看得到
//!     入口协议与出口协议」—— 插件只看到定稿后的 Chat 体，没有这件事可做；
//!   · **出站降级**（`is_native` / `downgrade`）：Chat 体里的原生声明**不能**
//!     发给一家只认 `type:"function"` 的上游。这一半必须在插件里做 —— 它就是
//!     构造上游请求的一部分。
//!
//! 所以这里只有降级所需的判据与剔除逻辑，函数体是网关那份的逐字副本。
//!
//! ── 为什么必须剔除（不是可选的清理）──────────────────────────
//! ZCode 的编码套餐端点只承载函数工具。客户端带上一条 `type:"web_search"`
//! 声明时：轻则上游忽略、重则整轮 400；而更糟的形态是客户端以为「搜索能用」，
//! 实际一次都没执行（issue #61 那类难查的故障）。剔除 + 留痕是唯一诚实的处置。
//!
//! ── 与网关那份的分工边界 ────────────────────────────────────
//! `restore`（同协议还原）与 `strip_cross_protocol`（跨协议剔除）**不在**这里：
//! 前者由网关在翻译机里做，后者的判据（来源标记是否等于目标协议）在插件侧
//! 恒成立 —— 目标就是 Chat，而带 `_wb_` 标记的声明来源必是别的协议。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::Value;

use crate::json::string_field;

/// 来源协议的暂存字段（与网关 `protocol::native_tool::FIELD_NATIVE_TOOL` 同值）。
pub(super) const FIELD_NATIVE_TOOL: &str = "_wb_native_tool";

/// 来源协议（只用于判「这条声明是从别的协议带过来的」）。
pub(super) fn origin_of(tool: &Value) -> Option<&str> {
    tool.get(FIELD_NATIVE_TOOL)
        .and_then(Value::as_str)
        .filter(|origin| !origin.trim().is_empty())
}

/// 是否原生声明：**带标记**的，或 `type` 存在且不是 `function` 的。
///
/// 第二条覆盖没挂标记的来路：chat 入口的请求体是原样透传的，客户端的
/// chat 方言原生工具（智谱那套 `{"type":"web_search", …}`）不会经过 `carry`。
pub(super) fn is_native(tool: &Value) -> bool {
    if origin_of(tool).is_some() {
        return true;
    }
    match tool {
        // 字符串简写只可能是宿主工具名（function 工具必须给参数表，写不成字符串）
        Value::String(_) => true,
        _ => {
            let kind = string_field(tool, "type");
            !kind.is_empty() && !kind.eq_ignore_ascii_case("function")
        }
    }
}

/// 一次降级的明细（供调用方留痕）。
pub(super) struct Downgrade {
    /// 被剔除的声明（原样，用于取名字与打印标签）。
    pub(super) tools: Vec<Value>,
    /// 被一并撤掉的 `tool_choice` 及原因（没有就是 None）。
    pub(super) choice: Option<String>,
}

impl Downgrade {
    /// 留痕文案（网关 `Downgrade::describe` 的精简版：插件侧没有
    /// `tool_plan::tool_kind_label`，退回按 `type` / `name` 打印）。
    pub(super) fn describe(&self, provider: &str, reason: Option<&str>) -> String {
        let mut text = String::new();
        if !self.tools.is_empty() {
            let labels: Vec<String> = self.tools.iter().map(label_of).collect();
            text.push_str(&format!(
                "⚠️ 剔除原生（服务端执行）工具声明 {} 条：{}",
                self.tools.len(),
                labels.join(", ")
            ));
            if let Some(reason) = reason {
                text.push_str(&format!("（{reason}）"));
            }
        }
        if let Some(choice) = &self.choice {
            if text.is_empty() {
                text.push_str(&format!("⚠️ 撤掉 tool_choice（{choice}）"));
            } else {
                text.push_str(&format!("；连带撤掉 tool_choice（{choice}）"));
            }
        }
        text.push_str(&format!("；provider={provider}"));
        text
    }
}

/// 一条声明的可读标签（`type` 优先，退回 `name`；字符串简写取自身）。
fn label_of(tool: &Value) -> String {
    match tool {
        Value::String(text) => text.clone(),
        other => {
            let kind = string_field(other, "type");
            if !kind.is_empty() {
                return kind;
            }
            let name = string_field(other, "name");
            if name.is_empty() {
                "未知".to_string()
            } else {
                name
            }
        }
    }
}

/// 声明指代的工具名（`name` 优先；没有 name 的宿主工具退回 `type`；
/// 字符串简写取自身）。`tool_choice` 的点名匹配用它。
fn name_of(tool: &Value) -> String {
    let name = string_field(tool, "name");
    if !name.is_empty() {
        return name;
    }
    match tool {
        Value::String(text) => text.trim().to_string(),
        other => string_field(other, "type"),
    }
}

/// 一条 chat 形态 `tool_choice` 指向的工具名（`auto` / `none` / `required`
/// 这类模式词不算点名）。
fn choice_target(choice: &Value) -> Option<String> {
    match choice {
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty()
                || matches!(text.to_ascii_lowercase().as_str(), "auto" | "none" | "required")
            {
                None
            } else {
                Some(text.to_string())
            }
        }
        _ => {
            let nested = choice
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if !nested.is_empty() {
                return Some(nested);
            }
            let flat = string_field(choice, "name");
            if flat.is_empty() {
                None
            } else {
                Some(flat)
            }
        }
    }
}

/// `tool_choice` 是否是原生工具的表达（chat 侧没有「强制调用宿主工具」的语义）。
fn is_native_choice(choice: &Value) -> bool {
    if choice.is_string() {
        return false;
    }
    let kind = string_field(choice, "type").to_ascii_lowercase();
    !kind.is_empty() && !matches!(kind.as_str(), "function" | "auto" | "none" | "required")
}

/// `tool_choice` 是否与本次剔除的声明冲突：是则返回一句原因。
///
/// 两种冲突：它本身指向原生工具；或它点名的工具正在被剔除。两种都必须撤掉 ——
/// 留着一条指向不存在工具的 `tool_choice`，上游会按「指定的工具不存在」把整轮
/// 打成 400。
pub(super) fn choice_conflict(choice: &Value, dropped: &[Value]) -> Option<String> {
    if is_native_choice(choice) {
        let kind = string_field(choice, "type");
        return Some(format!("它本身指向原生工具（type={kind}）"));
    }
    let name = choice_target(choice)?;
    dropped
        .iter()
        .any(|tool| name_of(tool) == name)
        .then(|| format!("它点名的「{name}」是本次被剔除的原生工具"))
}

/// 剔除**全部**原生声明（ZCode 的两条通道都只承载 `type:"function"` 的函数工具）。
///
/// 判据是 [`is_native`] —— 函数工具（`type:"function"`，含嵌套与扁平两种写法）
/// 一律保留。这里**不能**用恒真谓词：那会把客户端的函数工具一并删掉，模型拿到
/// 一个没有 tools 的请求，只能把调用写成正文吐出来。
///
/// 没有可剔的（绝大多数请求）返回 None；有则连带处理指向它们的 `tool_choice`。
#[allow(dead_code)]
// 整份降级（剔除全部原生声明）在插件里没有调用方：ZCode 的两条通道都走
// `anthropic_outbound::tool_to_anthropic` 的逐条判定（它自己收 `natives`）。
// 保留：那是「一次性剔除 + 留痕」的完整实现，与网关那份同构。
pub(super) fn downgrade(body: &Value) -> Option<(Value, Downgrade)> {
    let mut next = body.clone();
    let mut dropped: Vec<Value> = Vec::new();
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let mut kept: Vec<Value> = Vec::with_capacity(tools.len());
        for tool in tools {
            if is_native(tool) {
                dropped.push(tool.clone());
            } else {
                kept.push(tool.clone());
            }
        }
        if !dropped.is_empty() {
            if let Some(object) = next.as_object_mut() {
                if kept.is_empty() {
                    object.remove("tools");
                } else {
                    object.insert("tools".to_string(), Value::Array(kept));
                }
            }
        }
    }
    let mut choice: Option<String> = None;
    if let Some(current) = next.get("tool_choice").cloned() {
        if let Some(reason) = choice_conflict(&current, &dropped) {
            if let Some(object) = next.as_object_mut() {
                object.remove("tool_choice");
            }
            choice = Some(reason);
        }
    }
    if dropped.is_empty() && choice.is_none() {
        return None;
    }
    Some((next, Downgrade { tools: dropped, choice }))
}
