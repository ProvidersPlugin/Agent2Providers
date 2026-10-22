//! Anthropic Messages 协议的**出站**转换：chat 请求 → Anthropic 上游请求、
//! 上游 Anthropic SSE → chat SSE（自定义提供商转发的 `anthropic` 分支，
//! 以及内置 ZCode 活动套餐通道 —— 见 `upstream::translate` 的 `AnthropicToChatStream`）。
//!
//! ── 与 `anthropic.rs` 的方向关系 ─────────────────────────────
//! `anthropic.rs` 服务**下游入口**（`api::protocol` 的 `/v1/messages`）；
//! 本文件是它的反方向，服务自定义提供商与内置 ZCode 的**上游**：
//!   - 请求：`anthropic_request_from_chat`（chat 体 → Messages 体，逐字段
//!     对着 `chat_from_anthropic` 反推）；
//!   - 响应：`ChatFromAnthropicStream`（上游 Anthropic SSE → 标准 chat SSE，
//!     之后照走既有的 `ForwardStream` / 聚合器）。
//!
//! ── 三处结构差异的处理（与 `anthropic.rs` 模块头互为镜像）────
//!   1. chat 的 system 消息 → 顶层 `system` 字段（多条空行拼接）；
//!   2. chat 的 `role:"tool"` 消息 → user 消息里的 `tool_result` 块；
//!   3. chat 的 `message.tool_calls` → assistant 消息里的 `tool_use` 块
//!      （`arguments` 字符串解析成对象；Anthropic 的 input 必须是对象）。
//!
//! ── thinking 的两条 Anthropic 硬规则（都在这里守）────────────
//!   - `thinking.type:"enabled"` 要求 `budget_tokens < max_tokens`：
//!     注入时若 max_tokens 不够大，抬到 `budget + 1024`（参考实现同款）；
//!   - thinking 开启时 Anthropic 拒绝 `temperature` / `top_p`（temperature
//!     只允许 1）：与其让上游 400，不如**不带**这两个可选参数 —— 与
//!     `model_rules` 里「不向任何上游发会弄坏请求的字段」同一取向。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 同 `mod.rs`：零 unwrap/expect/panic；解析失败退化「跳过该帧」。

use serde_json::{json, Map, Value};

use crate::ids::random_id;
use crate::json::{content_parts, content_text, string_field, string_value};
use crate::reasoning::effort_rank;

use super::anthropic_shared::{
    parse_json_object, tool_result_parts, ToolResultParts, DEFAULT_MAX_TOKENS,
};

/// 请求体里的内部暂存字段（与网关 `protocol::mod` 的同名常量逐字相同）：
/// 客户端没给、由**入口**按协议带来的「缓存断点」与「这条 tool_result 是错误」。
/// 它们不发上游，只在两个方向之间传递语义。
const FIELD_CACHE_CONTROL: &str = "_wb_cache_control";
const FIELD_IS_ERROR: &str = "_wb_is_error";

// ─── 请求：Chat → Anthropic ─────────────────────────────────

/// Chat Completions 请求体 → Anthropic Messages 请求体。
///
/// ── 逐字段对着 [`super::anthropic::chat_from_anthropic`] 反推 ────
///   - `model`（由调用方给上游真名）
///   - `system` ← chat 的 system / developer 消息（多条空行拼接）
///   - `messages` ← 其余消息（tool 消息拆进 user 的 tool_result 块；
///     tool_calls 进 assistant 的 tool_use 块；连续同角色合并成一条 ——
///     Anthropic 要求 user / assistant 交替）
///   - `max_tokens` ← `max_tokens` / `max_completion_tokens`，缺省
///     [`DEFAULT_MAX_TOKENS`]（Anthropic 必填）
///   - `temperature` / `top_p` / `stop_sequences`（← chat `stop`）
///   - `tools`（chat 嵌套 function → `{name, description, input_schema}`）
///   - `tool_choice`（`required`→`any`、`{type:function,function:{name}}`
///     →`{type:"tool",name}`）
///   - `thinking` ← chat `reasoning_effort`（等级折算 budget，见
///     [`thinking_budget`]；同时可能抬高 max_tokens）
pub fn anthropic_request_from_chat(chat: &Value, model: &str) -> Result<Value, String> {
    let Some(messages_in) = chat.get("messages").and_then(Value::as_array) else {
        return Err("缺少 messages 数组".to_string());
    };
    let mut system: Vec<String> = Vec::new();
    // system 上的缓存断点（任一条 system 消息带断点即生效 —— 断点标记的是
    // 「缓存前缀到此为止」，多段 system 拼接后断点落在整段末尾）
    let mut system_cache: Option<Value> = None;
    let mut messages: Vec<Value> = Vec::new();
    for message in messages_in {
        let role = {
            let raw = string_field(message, "role").to_lowercase();
            if raw == "developer" {
                "system".to_string()
            } else if raw.is_empty() {
                "user".to_string()
            } else {
                raw
            }
        };
        if role == "system" {
            let text = content_text(message.get("content").unwrap_or(&Value::Null));
            if !text.trim().is_empty() {
                system.push(text);
                if let Some(cache) =
                    message.get(FIELD_CACHE_CONTROL).filter(|value| value.is_object())
                {
                    system_cache = Some(cache.clone());
                }
            }
            continue;
        }
        let blocks = anthropic_blocks_of(message, &role);
        if blocks.is_empty() {
            continue;
        }
        let target_role = if role == "assistant" { "assistant" } else { "user" };
        append_merged(&mut messages, target_role, blocks);
    }

    let mut out = Map::new();
    out.insert("model".to_string(), Value::String(model.to_string()));
    out.insert("messages".to_string(), Value::Array(messages));
    out.insert(
        "stream".to_string(),
        Value::Bool(chat.get("stream").and_then(Value::as_bool).unwrap_or(false)),
    );
    if !system.is_empty() {
        match system_cache {
            Some(cache) => {
                // 带断点的 system 用块数组形态（Anthropic 两种都认），
                // cache_control 落在最后一块 —— 与入口暂存时的位置一致
                let mut blocks: Vec<Value> = system
                    .iter()
                    .map(|text| json!({ "type": "text", "text": text }))
                    .collect();
                if let Some(last) = blocks.last_mut() {
                    if let Some(object) = last.as_object_mut() {
                        object.insert("cache_control".to_string(), cache);
                    }
                }
                out.insert("system".to_string(), Value::Array(blocks));
            }
            None => {
                out.insert("system".to_string(), Value::String(system.join("\n\n")));
            }
        }
    }

    // thinking 先判定：它决定 max_tokens 的下限，也决定 temperature/top_p
    // 能不能带（见模块头第二条规则）
    let thinking = chat
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .and_then(thinking_budget);
    let mut max_tokens = ["max_tokens", "max_completion_tokens"]
        .iter()
        .find_map(|key| chat.get(*key).and_then(Value::as_i64).filter(|value| *value > 0))
        .unwrap_or(DEFAULT_MAX_TOKENS);
    if let Some(budget) = thinking {
        // Anthropic 要求 budget_tokens < max_tokens；不够就抬到 budget 之上
        if max_tokens <= budget {
            max_tokens = budget + 1024;
        }
    }
    out.insert("max_tokens".to_string(), Value::from(max_tokens));
    if thinking.is_some() {
        if let Some(budget) = thinking {
            out.insert(
                "thinking".to_string(),
                json!({ "type": "enabled", "budget_tokens": budget }),
            );
        }
    } else {
        for key in ["temperature", "top_p"] {
            if let Some(value) = chat.get(key).filter(|value| !value.is_null()) {
                out.insert(key.to_string(), value.clone());
            }
        }
    }
    // stop → stop_sequences（chat 允许字符串或数组，Anthropic 只认数组）
    if let Some(stop) = chat.get("stop").filter(|value| is_truthy(value)) {
        let list = match stop {
            Value::Array(items) => items.clone(),
            other => vec![other.clone()],
        };
        if !list.is_empty() {
            out.insert("stop_sequences".to_string(), Value::Array(list));
        }
    }
    // 工具声明：函数工具翻译成 Anthropic 形态；原生（服务端执行）声明只有
    // 「来源就是 Anthropic」的原样恢复（保真），跨协议的不猜 —— 剔除并留痕
    // （见 `native_tool` 模块头；静默剔除或硬塞给上游都是 #61 那类难查的形态）
    let mut natives: Vec<Value> = Vec::new();
    if let Some(tools) = chat.get("tools").and_then(Value::as_array) {
        let converted: Vec<Value> = tools
            .iter()
            .filter_map(|tool| tool_to_anthropic(tool, &mut natives))
            .collect();
        if !converted.is_empty() {
            out.insert("tools".to_string(), Value::Array(converted));
        }
    }
    let mut choice_reason: Option<String> = None;
    if let Some(choice) = chat.get("tool_choice").filter(|value| is_truthy(value)) {
        // 点名的工具被剔除时 `tool_choice` 一并撤掉：留着它上游会按
        // 「指定的工具不存在」报错，把一次「搜索不可用」升级成整轮 400
        match super::native_tool::choice_conflict(choice, &natives) {
            Some(reason) => choice_reason = Some(reason),
            None => {
                out.insert("tool_choice".to_string(), tool_choice_to_anthropic(choice));
            }
        }
    }
    if !natives.is_empty() || choice_reason.is_some() {
        let dropped = super::native_tool::Downgrade { tools: natives, choice: choice_reason };
        crate::logging::log(
            "[Anthropic]",
            &dropped.describe("zcode", Some("目标上游按 anthropic 协议收，承载不了别的协议的原生类型")),
        );
    }
    Ok(Value::Object(out))
}

/// 拆开的工具结果 → Anthropic 的 `tool_result.content`。
///
/// ── 与入站方向相反的取舍（不是笔误）──────────────────────────
/// 入站（`anthropic.rs`）图片**必须**挪出 tool 消息：Chat 不许 `tool` 角色带
/// 图片（OpenAI 直接 400，见 `responses::PendingImages`），所以那边图片落到
/// 相邻的 user 消息上。出站这边不用搬 —— Anthropic 的 `tool_result.content`
/// 本来就接受嵌套内容块（官方收 `text` / `image` / `document` / `search_result`），
/// 图片留在结果里才是保真形态。
///
/// 没有图片时保持字符串形态：字符串对各家上游最友好，也是原来就在发的形状。
fn tool_result_content(parts: ToolResultParts) -> Value {
    if parts.images.is_empty() {
        return Value::String(parts.text);
    }
    let mut blocks: Vec<Value> = Vec::new();
    if !parts.text.is_empty() {
        blocks.push(json!({ "type": "text", "text": parts.text }));
    }
    blocks.extend(parts.images);
    Value::Array(blocks)
}

/// 一条非 system 的 chat 消息 → Anthropic 内容块数组。
fn anthropic_blocks_of(message: &Value, role: &str) -> Vec<Value> {
    // tool 消息 → tool_result 块（挂在 user 消息上；Anthropic 要求
    // tool_result 与 assistant 的 tool_use 成对，tool_use_id 必须指向
    // 真实存在的 id —— 缺 id 说明客户端数据本身缺配对，凭空补一个只会让
    // 上游的配对校验指向更莫名其妙的块，所以原样发、让上游如实报错）
    if role == "tool" {
        // is_error 恢复：入口翻译时暂存的「工具执行失败」标记（见
        // `anthropic::convert_message`）—— OpenAI 形出口没有这个概念，
        // 那条路径随 strip 剥离，只有这里能把它带回去
        let is_error = message.get(FIELD_IS_ERROR).and_then(Value::as_bool) == Some(true);
        let cache = message.get(FIELD_CACHE_CONTROL).filter(|value| value.is_object());
        let mut block = json!({
            "type": "tool_result",
            "tool_use_id": string_field(message, "tool_call_id"),
            "content": tool_result_content(tool_result_parts(
                message.get("content").unwrap_or(&Value::Null),
                image_to_anthropic,
            )),
        });
        if let Some(object) = block.as_object_mut() {
            if is_error {
                object.insert("is_error".to_string(), Value::Bool(true));
            }
            if let Some(cache) = cache {
                object.insert("cache_control".to_string(), cache.clone());
            }
        }
        return vec![block];
    }
    let mut blocks: Vec<Value> = Vec::new();
    if role == "assistant" {
        // 思考 → thinking 块（不带 signature：Chat 侧没有签名概念，
        // 见 `anthropic.rs` 模块头的方向约定）
        let reasoning = {
            let from_field = string_field(message, "reasoning_content");
            if from_field.is_empty() {
                string_field(message, "reasoning")
            } else {
                from_field
            }
        };
        if !reasoning.is_empty() {
            blocks.push(json!({ "type": "thinking", "thinking": reasoning }));
        }
    }
    // 正文：字符串 / 块数组（图片 → image 块，见 [`image_to_anthropic`]）
    match message.get("content").unwrap_or(&Value::Null) {
        Value::String(text) => {
            if !text.is_empty() {
                blocks.push(json!({ "type": "text", "text": text }));
            }
        }
        content => {
            for part in content_parts(content) {
                if let Some(text) = part.as_str() {
                    if !text.is_empty() {
                        blocks.push(json!({ "type": "text", "text": text }));
                    }
                    continue;
                }
                let kind = string_field(part, "type").to_lowercase();
                match kind.as_str() {
                    "text" | "input_text" | "output_text" => {
                        let text = string_field(part, "text");
                        if !text.is_empty() {
                            blocks.push(json!({ "type": "text", "text": text }));
                        }
                    }
                    "image_url" | "input_image" if role != "assistant" => {
                        if let Some(image) = image_to_anthropic(part) {
                            blocks.push(image);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    // 工具调用 → tool_use 块（input 必须是对象；`arguments` 解析不出时
    // 包一层 raw，与 `anthropic.rs` 的 `parse_json_object` 同口径）
    if role == "assistant" {
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let id = {
                    let raw = string_field(call, "id");
                    if raw.is_empty() {
                        random_id("toolu")
                    } else {
                        raw
                    }
                };
                blocks.push(json!({
                    "type": "tool_use",
                    "id": id,
                    "name": call.pointer("/function/name").map(string_value).unwrap_or_default(),
                    "input": parse_json_object(
                        call.pointer("/function/arguments").unwrap_or(&Value::Null)
                    ),
                }));
            }
        }
    }
    // 消息级缓存断点 → 落回最后一个可挂载的块。Anthropic 不允许把
    // cache_control 挂在 thinking 块上，跳过；没有可挂的块时整段放弃
    // （给上游发一个非法位置的断点只会换来 400）
    if let Some(cache) = message.get(FIELD_CACHE_CONTROL).filter(|value| value.is_object()) {
        for block in blocks.iter_mut().rev() {
            let kind = string_field(block, "type");
            let mountable = matches!(kind.as_str(), "text" | "tool_use" | "tool_result" | "image");
            if mountable {
                if let Some(object) = block.as_object_mut() {
                    object.insert("cache_control".to_string(), cache.clone());
                }
                break;
            }
        }
    }
    blocks
}

/// 连续同角色的消息合并成一条（Anthropic 要求 user / assistant 交替，
/// 拆开的两条同角色消息会被上游拒）。
fn append_merged(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if let Some(last) = messages.last_mut() {
        let same_role = last.get("role").and_then(Value::as_str) == Some(role);
        if same_role {
            if let Some(existing) = last.get_mut("content").and_then(Value::as_array_mut) {
                existing.extend(blocks);
                return;
            }
        }
    }
    messages.push(json!({ "role": role, "content": blocks }));
}

/// Chat 的 image_url 块 → Anthropic 的 image 块。
///
/// 两个来源都要认：`data:` URI（解码出 media_type 与 base64 数据）→
/// `source:{type:"base64"}`；http(s) 外链 → `source:{type:"url"}`。
/// 其余形态（相对路径、非法 data URI）丢弃 —— 发一个上游读不懂的 source
/// 只会让整条请求 400。
fn image_to_anthropic(part: &Value) -> Option<Value> {
    let source = part.get("image_url").unwrap_or(part);
    let url = match source {
        Value::String(text) => text.clone(),
        other => string_field(other, "url"),
    };
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',')?;
        // `data:<media_type>;base64,<data>`；没有 `;base64` 后缀的形态
        // （非 base64 编码）Anthropic 不接受，丢弃
        let media_type = meta.strip_suffix(";base64")?;
        if data.is_empty() {
            return None;
        }
        let media_type = if media_type.is_empty() {
            "image/png".to_string()
        } else {
            media_type.to_string()
        };
        return Some(json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media_type, "data": data },
        }));
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(json!({
            "type": "image",
            "source": { "type": "url", "url": url },
        }));
    }
    None
}

/// Chat 工具声明 → Anthropic 工具（`input_schema` 形态）。
///
/// 原生（服务端执行）声明分两种归宿：来源是 Anthropic 的原样恢复（`natives`
/// 不收，保真）；其余（Responses 来源、或 chat 入口的方言原生工具）收进
/// `natives` 由调用方留痕剔除 —— 目标协议是 anthropic，承载不了别的协议的
/// 原生类型（见 `native_tool` 模块头）。
fn tool_to_anthropic(tool: &Value, natives: &mut Vec<Value>) -> Option<Value> {
    if super::native_tool::is_native(tool) {
        // 插件这一侧**不做 restore**：带 `_wb_` 来源标记的声明是网关从
        // Responses / Anthropic 入口带过来的，而插件只看到定稿的 Chat 体 ——
        // 它没有「目标协议是否与来源相同」这件事可判（目标是 Anthropic 通道
        // 时，`anthropic` 来源理论上可以还原，但那条还原路径在网关的翻译机里
        // 已经走过一遍，这里再还一次会把两处口径分叉）。
        // 统一收进 `natives` 留痕剔除，是「不猜」那一侧的选择。
        natives.push(tool.clone());
        return None;
    }
    // chat 侧只会有嵌套形态；裸 function 对象也容忍（两种形态等价）
    let function = tool.get("function").unwrap_or(tool);
    let name = string_field(function, "name");
    if name.is_empty() {
        return None;
    }
    let mut out = Map::new();
    out.insert("name".to_string(), Value::String(name));
    if let Some(description) = function.get("description").filter(|value| is_truthy(value)) {
        out.insert("description".to_string(), description.clone());
    }
    let schema = function.get("parameters").unwrap_or(&Value::Null);
    out.insert("input_schema".to_string(), normalize_input_schema(schema));
    // 工具定义上的缓存断点随内部暂存字段恢复（工具清单大而稳定，是
    // Anthropic 提示缓存收益最高的一段；入口侧见 `anthropic::tool_to_chat`）
    if let Some(cache) = tool.get(FIELD_CACHE_CONTROL).filter(|value| value.is_object()) {
        out.insert("cache_control".to_string(), cache.clone());
    }
    Some(Value::Object(out))
}

/// `input_schema` 归一：Anthropic 要求顶层 `type:"object"`、`properties`
/// 存在（口径对齐参考实现 `normalizeAnthropicInputSchema`）。
fn normalize_input_schema(schema: &Value) -> Value {
    let kind = string_field(schema, "type").to_lowercase();
    if !schema.is_object() || (!kind.is_empty() && kind != "object") {
        return json!({ "type": "object", "properties": {} });
    }
    let mut out = schema.as_object().cloned().unwrap_or_default();
    out.insert("type".to_string(), Value::String("object".to_string()));
    out.entry("properties".to_string()).or_insert_with(|| json!({}));
    Value::Object(out)
}

/// Chat 的 tool_choice → Anthropic 的 tool_choice（`chat_from_anthropic`
/// 的 `tool_choice_to_chat` 的反向表：`required`↔`any`、function↔tool）。
fn tool_choice_to_anthropic(choice: &Value) -> Value {
    if let Some(text) = choice.as_str() {
        return match text {
            "required" => json!({ "type": "any" }),
            "none" => json!({ "type": "none" }),
            // `auto` 与其余未识别的字符串都落到 auto（Anthropic 只认这四个
            // 形态；发一个它不认识的字符串是必然的 400）
            _ => json!({ "type": "auto" }),
        };
    }
    let name = {
        let nested = choice.pointer("/function/name").map(string_value).unwrap_or_default();
        if nested.is_empty() {
            string_field(choice, "name")
        } else {
            nested
        }
    };
    if name.is_empty() {
        return json!({ "type": "auto" });
    }
    json!({ "type": "tool", "name": name })
}

/// chat 的 `reasoning_effort` → Anthropic 的 `budget_tokens`。
///
/// 一档一个值，按 [`model_rules::reasoning_rank`] 的强弱序折算；表外等级
/// （自定义输入）没有可翻译的目标，返回 None（与 `model_rules` 的
/// 「表外值不参与转发」同一闸门）：
///
/// ```text
///   rank 0..1（minimal / low）→ 1024
///   rank 2    （medium）      → 4096
///   rank 3    （high）        → 10240
///   rank 4..5（xhigh / max）  → 32768
/// ```
///
/// 分档值与参考实现 `thinkingBudget` 对齐（它的 low/medium/high/max 四个
/// 值原样落在这四档上）。
fn thinking_budget(effort: &str) -> Option<i64> {
    let rank = effort_rank(effort)?;
    Some(match rank {
        0 | 1 => 1024,
        2 => 4096,
        3 => 10240,
        _ => 32768,
    })
}

/// JS 真值判定（与网关 `protocol::is_truthy` 逐字相同）。
fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|item| item != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// JSON 文本化（与网关 `protocol::json_text` 逐字相同）。
#[allow(dead_code)]
// 本文件里的 `json_text` 目前没有直接调用方（`parse_json_object` 用的是
// `anthropic_shared` 里那份）。保留它是因为它和 `is_truthy` 是这一层成对的
// 小工具，删一个留一个反而难读。
fn json_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}
