//! 国际版系统提示的纯文本兼容层，不改用户、工具、图片或轮次。
//! 官方国际客户端当前使用 AutoClaw 单句前缀（plugins#367 的产物核对与回执）。
//! 旧 OpenClaw 前缀及 Tooling 段完整保留在新前缀后的原系统正文中。
//! 已确认的外来身份句只在 system/developer 文本中归一（plugins#924、#1094）。
//! 不删除全部系统提示，也不把鉴权、奖励或内容拒绝当作可绕过的检查。

use serde_json::{json, Value};

/// 新国际客户端系统首句；已有前缀只归一身份措辞，不重复前置。
pub const IDENTITY_LINE: &str = "You are AutoClaw. Answer the user directly and concisely.";

/// 末尾带一个换行；join_prefix 留空行后接回全部原系统正文。
const IDENTITY_PREFIX: &str = "You are AutoClaw. Answer the user directly and concisely.\n";

/// 外来身份句只改文本载体，长规则先于短规则；匹配忽略 ASCII 大小写。
/// 固定规则来自既有实现和已核实公开修复，不处理用户/工具消息。
const FOREIGN_IDENTITIES: &[(&str, &str)] = &[
    // ── ZCode（长的在前）──
    (
        "You are ZCode, an interactive coding agent",
        "You are an interactive coding agent",
    ),
    ("You are ZCode", "You are an interactive coding agent"),
    ("an agent for ZCode", "an agent for a coding assistant"),
    // ── Codex（三种长句式在前，`You are Codex` / `Codex CLI` 兜底在后）──
    (
        "You are a coding agent running in the Codex CLI tool",
        "You are a coding agent running in a terminal CLI tool",
    ),
    (
        "You are a coding agent running in the Codex CLI",
        "You are a coding agent running in a terminal CLI",
    ),
    (
        "You are running as a coding agent in the Codex CLI",
        "You are running as a coding agent in a terminal CLI",
    ),
    ("You are Codex", "You are a coding agent"),
    ("Codex CLI", "Codex command-line tool"),
    // ── Claude 系（Claude Code 2.1.30x 起首句换成了 SDK 身份句，长在前）──
    (
        "You are a Claude agent, built on Anthropic's Claude Agent SDK",
        "You are a coding assistant",
    ),
    ("Claude Agent SDK", "the agent framework"),
    // Claude Code 的整句是 "You are Claude Code, Anthropic's official CLI tool for
    // Claude."；只改到「You are Claude Code」为止，后面的产品说明原样保留。
    ("You are Claude Code", "You are a coding assistant"),
    (
        "an agent for Claude Code",
        "an agent for a coding assistant",
    ),
    // ── DeepSeek Harness（国际版，issue #10）──
    (
        "You are an AI agent powered by DeepSeek Harness",
        "You are an AI agent powered by a local coding harness",
    ),
    // 国际版 system 中后段也可能含该品牌；只改首句不足以兼容（plugins#924）。
    ("DeepSeek Harness", "a local coding harness"),
    // ── 其它 CLI / Agent 身份句（2026-11 照搬 magpie 的 autoclaw 改写清单）──
    ("You are CodeBuddy Code", "You are a coding assistant"),
    ("You are Hermes Agent", "You are an agent"),
    ("You are Hermes", "You are an agent"),
    ("You are Gemini CLI", "You are a coding agent"),
    ("You are Kimi Code CLI", "You are a coding agent"),
    ("You are Qwen Code", "You are a coding agent"),
    ("You are opencode", "You are a coding agent"),
    ("You are Cursor", "You are a coding agent"),
    ("You are Cline", "You are a coding agent"),
    ("You are goose", "You are a coding agent"),
    // ── 其它措辞：pi 的描述句、ZCode 的「powered by GLM-<版本>」开场白 ──
    (
        "operating inside pi, a coding agent harness",
        "operating inside a coding agent harness",
    ),
    (
        "This conversation is powered by GLM",
        "This conversation is powered by the GLM",
    ),
];

/// 未解析的 406 只说明上游拒收，不推导为额度或登录态失效。
pub const UPSTREAM_406_HINT: &str = "；国际版上游拒收（406）且未给出原因。系统提示与请求头已按当前客户端规范处理，\
不被接受的 max_tokens/max_completion_tokens 不会发送；如果仍复现，请提供脱敏请求以核对，或改用其它提供商。\
不会把未知拒收当成额度不足或自动修改用户对话。";

/// 出站前规范化 body 里的系统提示词（**就地修改**）。
///
/// 两步：① 所有 system / developer 消息里的外来身份句改写；② 保证首条消息是
/// system / developer 且正文以身份句开头（是则前置前缀，否则在 `messages[0]`
/// 插一条带前缀的 system 消息）。
///
/// `messages` 缺失或不是数组时**直接返回**：那不是本模块能修的形态，交给上游报
/// 格式错误比在这里造一个空 `messages` 更能说明问题（与 `adapter` 对非对象 body
/// 的处置同一取向）。
pub fn normalize(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages.iter_mut() {
        if !is_system_role(message) {
            continue;
        }
        if let Some(content) = message.get_mut("content") {
            rewrite_identities_in_place(content);
        }
    }
    // 首条就是 system / developer：前缀拼进它自己的正文（客户端提示词保留在后）。
    if messages.first().map(is_system_role).unwrap_or(false) {
        if let Some(first) = messages.first_mut() {
            prepend_prefix(first);
        }
        return;
    }
    // 首条不是：补一条只带前缀的 system 消息。**不改**后面那些 system 消息的
    // 位置 —— 重排消息序列的风险（破坏客户端的轮次结构）远大于收益。
    messages.insert(0, json!({ "role": "system", "content": IDENTITY_PREFIX }));
}

/// 给一条消息的 `content` 前置身份前缀（已经以身份句开头则不动）。
///
/// `content` 的三种形态：
///   - 字符串：直接拼；
///   - 数组（多 part）：拼进**第一个文本 part**；一个文本 part 都没有时在数组
///     头部插一个 —— 上游闸门看的是文本，插一个 part 比替换整个数组安全；
///   - 其它（null / 对象 / 数字）：换成前缀本身。这种 content 当 system 提示词
///     本来就不成立（上游会当空提示词处理，稳定 406），换掉比原样发出去好。
fn prepend_prefix(message: &mut Value) {
    let Some(content) = message.get_mut("content") else {
        // 连 content 字段都没有：补上（`role` 已经是 system 了，补 content 不会
        // 改变这条消息的语义，只是让闸门能过）
        if let Some(object) = message.as_object_mut() {
            object.insert(
                "content".to_string(),
                Value::String(IDENTITY_PREFIX.to_string()),
            );
        }
        return;
    };
    match content {
        Value::String(text) => {
            if !text.starts_with(IDENTITY_LINE) {
                let next = join_prefix(text);
                *text = next;
            }
        }
        Value::Array(parts) => {
            let index = parts
                .iter()
                .position(|part| part.get("text").and_then(Value::as_str).is_some());
            match index {
                Some(index) => {
                    let already = parts
                        .get(index)
                        .and_then(|part| part.get("text"))
                        .and_then(Value::as_str)
                        .map(|text| text.starts_with(IDENTITY_LINE))
                        .unwrap_or(false);
                    if already {
                        return;
                    }
                    // `index` 是上面按 `as_str().is_some()` 选出来的，所以这里的
                    // `get_mut` 必然是字符串 —— `if let` 不是「可能静默跳过」的
                    // 分支，而是没有 `as_str_mut` 可用时的取值写法。
                    if let Some(Value::String(text)) =
                        parts.get_mut(index).and_then(|part| part.get_mut("text"))
                    {
                        let next = join_prefix(text);
                        *text = next;
                    }
                }
                None => parts.insert(0, json!({ "type": "text", "text": IDENTITY_PREFIX })),
            }
        }
        other => {
            *other = Value::String(IDENTITY_PREFIX.to_string());
        }
    }
}

/// 前缀 + 一个空行 + 原文（原文为空时不留多余空行）。
fn join_prefix(original: &str) -> String {
    if original.is_empty() {
        return IDENTITY_PREFIX.to_string();
    }
    let mut out = String::with_capacity(IDENTITY_PREFIX.len() + original.len() + 1);
    out.push_str(IDENTITY_PREFIX);
    out.push('\n');
    out.push_str(original);
    out
}

/// 就地改写 content（字符串 / 文本 part 数组）里的外来身份句。
///
/// 只处理这两种承载文本的形态：`content` 是别的东西（null / 对象）时无文本可改，
/// 由 [`prepend_prefix`] 统一处置。
fn rewrite_identities_in_place(content: &mut Value) {
    match content {
        Value::String(text) => rewrite_identities(text),
        Value::Array(parts) => {
            for part in parts.iter_mut() {
                if let Some(Value::String(text)) = part.get_mut("text") {
                    rewrite_identities(text);
                }
            }
        }
        _ => {}
    }
}

/// 逐条套用 [`FOREIGN_IDENTITIES`]（**全量**替换，一条文本里出现多次也一并改掉）。
///
/// 匹配忽略 ASCII 大小写 —— 闸门自己就不敏感（见模块头的国际版实测），替换也
/// 必须不敏感：否则同一句指纹换个大小写就原样出站被拦。
fn rewrite_identities(text: &mut String) {
    for (from, to) in FOREIGN_IDENTITIES {
        if let Some(next) = replace_all_ignore_ascii_case(text, from, to) {
            *text = next;
        }
    }
    normalize_model_sentence(text);
}

// 只处理已确认的 GLM 身份句式（plugins#924），模型ID、文件名和其它正文保持。
fn normalize_model_sentence(text: &mut String) {
    let needle = "powered by the model glm-";
    let mut rest = text.as_str();
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    while let Some(start) = find_ignore_ascii_case(rest, needle) {
        let tail = &rest[start + needle.len()..];
        let end = tail
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(byte))
            .count();
        if end == 0 {
            out.push_str(&rest[..start + needle.len()]);
            rest = tail;
            continue;
        }
        out.push_str(&rest[..start]);
        out.push_str("powered by the GLM-");
        let version = tail[..end].trim_end_matches('.');
        out.push_str(version);
        out.push_str(" model");
        out.push_str(&tail[version.len()..end]);
        rest = &tail[end..];
        changed = true;
    }
    out.push_str(rest);
    if changed {
        *text = out;
    }
}

/// `from` 在 `text` 里的**全量**替换（忽略 ASCII 大小写）；一次都没命中返回
/// `None`（调用方据此跳过写回，与「不命中就不动原串」同一取向）。
///
/// 替换串 `to` 里即使含 `from` 也不会死循环：每轮从匹配处**之后**继续扫，
/// 替换出来的字节永不被重新检查。
fn replace_all_ignore_ascii_case(text: &str, from: &str, to: &str) -> Option<String> {
    find_ignore_ascii_case(text, from)?;
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = find_ignore_ascii_case(rest, from) {
        out.push_str(&rest[..start]);
        out.push_str(to);
        rest = &rest[start + from.len()..];
    }
    out.push_str(rest);
    Some(out)
}

/// `from` 在 `text` 里首次出现的**字节下标**（只折叠 ASCII 大小写）。
///
/// 为什么不是 `text.to_lowercase()` 再找：Unicode 折叠会改变字节长度（`İ` 折叠
/// 出来是两个字符），折叠后的下标回不到原文，切片就会错位。这里按字节滑窗加
/// `eq_ignore_ascii_case` 比较：`from` 全是 ASCII，而 UTF-8 里非 ASCII 字符的每个
/// 字节都 >= 0x80，折叠后不可能等于 ASCII 字节 —— 于是命中区间只可能由 ASCII
/// 字节组成，起止点都是字符边界，切片安全（本文件 `panic=abort`，这里连一个
/// `unwrap` 都不需要）。
fn find_ignore_ascii_case(text: &str, from: &str) -> Option<usize> {
    let haystack = text.as_bytes();
    let needle = from.as_bytes();
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len())
        .find(|&start| haystack[start..start + needle.len()].eq_ignore_ascii_case(needle))
}

/// 该消息是否承载 system 级指令：角色**精确**等于 `system` 或 `developer`。
///
/// 与 `core::prompt::is_system_role` 同一口径（那边是私有函数，本目录不跨模块
/// 借用）：精确匹配而不是前缀 / 包含，其余角色（含未知值）一律不动 —— 这是
/// 「不动用户内容」这条边界的判据。
fn is_system_role(message: &Value) -> bool {
    message
        .get("role")
        .and_then(Value::as_str)
        .map(|role| role == "system" || role == "developer")
        .unwrap_or(false)
}
