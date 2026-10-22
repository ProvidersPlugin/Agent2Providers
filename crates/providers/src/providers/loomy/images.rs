//! Loomy 上游的图片格式适配：哪些模型要把 `image_url` 换成 `<image_base64>` 标记。
//!
//! ── 上游按模型认两种图片形态（直连 loomyad 实测，2026-10）────
//! 分界线是**上游目录 `/api/v1/models` 自己声明的 `vision` 位**，不是模型名：
//!   - `vision: true`（`DeepSeek-V4.1-Flash` / `GLM-5.3-Flash` / `Kimi-k2.6` …）：
//!     认标准 OpenAI `image_url` data URI，图片按**视觉 token** 计费、与 base64
//!     长度无关 —— 实测 128×128 同一张图，348 字符与 65800 字符两次请求
//!     prompt 都恰好是 241 token。
//!   - `vision: false`（`deepseek-v4-flash-0731` / `qwen-3.8-max` / `spark-x`）：
//!     上游没有视觉通路，`image_url` 里的 base64 只会被当文本喂进去。标记写法
//!     （`<image_base64>…</image_base64>`）是这一档的兜底 —— 上游把标记转成
//!     视觉 token：同一张图 prompt 164 → 7650，模型能答出画面内容。
//!
//! ── 判据为什么不能用模型名字面量（2026-10-17 修正）────────────
//! 原先用 `contains("deepseek")` 一把抓，把**声明支持图片的**
//! `DeepSeek-V4.1-Flash` 也按只认标记的 0731 处理了：两张 JPEG 被改写成
//! 82.8 万字符的 base64 文本，prompt 从 33.2 万涨到 65.8 万（用户报的
//! 「切到 Loomy 上下文翻倍」），而且这段文本每轮对话都要重发。
//!
//! ── 标记并入相邻文本块（有实测依据的形态）────────────────────
//! 直连验证通过的形态是「一段文本 + 标记写在同一段里」（content 为字符串）；
//! 数组里独立 text 块给上游的形态没有实测依据，所以这里把标记并入前一个
//! text 块（没有前块时并入后块、再没有才独立成块）。

use serde_json::{json, Value};

use super::models;

/// 转发前对图片做的适配：返回 (要发出去的 body, 转换的图片数)。
///
/// 无图请求（绝大多数）只付一次借用扫描就返回 —— 不深拷贝、不查目录。
pub fn adapt_images(body: &Value, requested_model: &str) -> (Value, usize) {
    if !has_convertible_image(body) {
        return (body.clone(), 0);
    }
    if !needs_tag_format(requested_model) {
        return (body.clone(), 0);
    }
    rewrite_to_tag(body)
}

/// 该请求的图片要不要转成 `<image_base64>` 标记（判据与依据见模块头）。
///
/// ── 为什么直接看 `requested_model` 而不再解析 wire ──────────
/// 网关在调 `build_chat` **之前**已经把 `body.model` 改写成本家要收的名字
/// （见网关 `upstream::payload::send_body`：先 `rewrite_model`、再问插件）。
/// 所以这里读的已经是发送名；再自己解析一次只会多一条可能与网关分叉的路径。
pub fn needs_tag_format(model: &str) -> bool {
    match models::declared_image_input(model) {
        Some(supported) => !supported,
        // 目录里查不到（还没刷出来 / 自定义模型）：回落旧的模型名字面量判据，
        // 冷启动不该静默改变转发形态。
        None => legacy_name_rule(model),
    }
}

/// 旧判据（模型名里含 `deepseek`）：只在目录查不到这个模型时兜底。
fn legacy_name_rule(model: &str) -> bool {
    model.to_ascii_lowercase().contains("deepseek")
}

/// 把 messages 里的 data URI 图片块转成 `<image_base64>` 标记。返回
/// (改写后的 body, 转换的图片数)；没有可转的图片时返回原 body 的克隆 + 0。
pub fn rewrite_to_tag(body: &Value) -> (Value, usize) {
    if !has_convertible_image(body) {
        return (body.clone(), 0);
    }
    let mut next = body.clone();
    let mut converted = 0usize;
    let Some(messages) = next.get_mut("messages").and_then(Value::as_array_mut) else {
        return (next, 0);
    };
    for message in messages.iter_mut() {
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        // 倒序一轮：替换/合并会改变下标，从后往前才不会被已处理的位置带偏
        let mut index = content.len();
        while index > 0 {
            index -= 1;
            let Some(tag) = to_tag_text(&content[index]) else {
                continue;
            };
            content.remove(index);
            if let Some(text) = index
                .checked_sub(1)
                .and_then(|previous| text_part_mut(&mut content[previous]))
            {
                // 优先并入**前一个** text 块：与原消息的文本顺序一致，
                // 「这条文本在说这张图」的关系保持最近
                text.push('\n');
                text.push_str(&tag);
            } else if let Some(text) = content.get_mut(index).and_then(text_part_mut) {
                text.insert_str(0, &format!("{tag}\n"));
            } else {
                content.insert(index, json!({ "type": "text", "text": tag }));
            }
            converted += 1;
        }
    }
    (next, converted)
}

/// messages 里有没有至少一个可转的 data URI 图片块（借用扫描，不拷贝）
fn has_convertible_image(body: &Value) -> bool {
    body.get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages.iter().any(|message| {
                message
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|content| content.iter().any(|part| to_tag_text(part).is_some()))
            })
        })
}

/// 单个图片块 → `<image_base64>…</image_base64>` 标记文本。
/// 只认 data URI（`data:image/…;base64,…`）；http(s) 图片 URL 不转 ——
/// 标记里放 URL 没有意义，上游认不认又没有实测依据，保持原样最保守。
fn to_tag_text(part: &Value) -> Option<String> {
    let kind = part
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if kind != "image_url" && kind != "input_image" {
        return None;
    }
    // 两种 url 形态都认：`{"image_url":{"url":"data:…"}}`（OpenAI 标准）
    // 与 `{"image_url":"data:…"}`（宽口径客户端的简化写法）
    let url = part
        .get("image_url")
        .and_then(|value| {
            value
                .get("url")
                .and_then(Value::as_str)
                .or_else(|| value.as_str())
        })
        .map(str::trim)
        .filter(|url| !url.is_empty())?;
    let base64 = data_uri_base64(url)?;
    Some(format!("<image_base64>{base64}</image_base64>"))
}

/// `data:image/png;base64,AAAA…` → `AAAA…`（非 data URI / 非 base64 → None）
fn data_uri_base64(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    if !meta.to_ascii_lowercase().contains("base64") {
        return None;
    }
    let payload = payload.trim();
    if payload.is_empty() {
        return None;
    }
    Some(payload)
}

/// text 块的可写文本（不是 `{type:"text", text:"…"}` 形态时 None）
fn text_part_mut(part: &mut Value) -> Option<&mut String> {
    let is_text = part
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
        == Some("text");
    if !is_text {
        return None;
    }
    match part.get_mut("text") {
        Some(Value::String(text)) => Some(text),
        _ => None,
    }
}
