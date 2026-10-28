//! 会话式转发：三步链路 + run 复用 + SSE 翻译。
//!
//! ── 为什么要缓存 run ────────────────────────────────────────
//! 灵犀计费按 **run**：一个 run 内多次 `chat/completions` 只扣一次智点。
//! 每次请求都新建 run 会让「一个会话几十轮」变成几十智点（实测一个 run
//! 内跨 5 分钟、含并发也只扣 1）。所以按「账号 × 客户端会话 × 模型」缓存
//! `session_id` + `run_id`，TTL 一小时；上游报 `SQLNoRows`（会话/run 已失效）
//! 时丢弃缓存、重建一次再重试。
//!
//! ── 为什么客户端会话为空也能用 ──────────────────────────────
//! 有些客户端不发会话头；此时用「账号 + 模型」当键 —— 消息每轮都是全量下发，
//! 复用 run 只影响计费与上游的会话归属，不影响上下文正确性。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ConversationSink, PluginCtx};
use serde_json::{json, Map, Value};

use super::account;
use super::constants;
use super::http;
use super::stream::Translator;

#[derive(Clone)]
struct CachedRun {
    session_id: String,
    run_id: String,
    at: i64,
}

static RUNS: OnceLock<Mutex<HashMap<String, CachedRun>>> = OnceLock::new();

fn runs() -> &'static Mutex<HashMap<String, CachedRun>> {
    RUNS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or(0)
}

fn cached(key: &str) -> Option<CachedRun> {
    let guard = runs().lock().ok()?;
    let run = guard.get(key)?;
    (now_ms() - run.at < constants::RUN_TTL_MS).then(|| run.clone())
}

fn remember(key: &str, run: &CachedRun) {
    if let Ok(mut guard) = runs().lock() {
        guard.insert(key.to_string(), run.clone());
    }
}

fn forget(key: &str) {
    if let Ok(mut guard) = runs().lock() {
        guard.remove(key);
    }
}

/// 一次失败：`retry_run` 表示「会话 / run 失效，重建后可以再试一次」。
struct Failure {
    message: String,
    retry_run: bool,
}

impl Failure {
    fn other(message: impl Into<String>) -> Self {
        Self { message: message.into(), retry_run: false }
    }

    fn run(message: impl Into<String>) -> Self {
        Self { message: message.into(), retry_run: true }
    }
}

/// 判断上游错误是不是「会话 / run 不存在」（可以换一个新的再试）。
fn looks_like_stale_run(message: &str) -> bool {
    let lowered = message.to_lowercase();
    lowered.contains("sqlnorows")
        || lowered.contains("no rows")
        || message.contains("会话不存在")
        || message.contains("数据不存在")
        || (lowered.contains("session") && lowered.contains("not found"))
}

/// 执行一次会话式转发。
pub async fn conversation(
    ctx: &PluginCtx,
    req: proto::ConversationRequest,
    sink: ConversationSink<'_>,
) -> Result<(), String> {
    let sid = account::sid_for(ctx, &req.account).await?;
    let client = http::build_client(req.proxy.as_ref(), &ctx.env.proxy);

    let requested = if !req.wire_model.trim().is_empty() {
        req.wire_model.clone()
    } else {
        req.body.get("model").and_then(Value::as_str).unwrap_or("").to_string()
    };
    let model = constants::wire_key(&requested);
    let session = if req.session.trim().is_empty() {
        format!("acct-{}", req.account)
    } else {
        req.session.clone()
    };
    let key = format!("{}|{}|{}", req.account, session, model);

    let mut sink = sink;
    let mut attempt = 0;
    loop {
        let run = ensure_run(&client, &sid, &key, &model, &req.body).await?;
        match forward(&client, &sid, &run, &model, &req.body, &mut sink).await {
            Ok(()) => return Ok(()),
            Err(failure) => {
                if failure.retry_run && attempt == 0 {
                    forget(&key);
                    attempt += 1;
                    continue;
                }
                return Err(failure.message);
            }
        }
    }
}

async fn ensure_run(
    client: &reqwest::Client,
    sid: &str,
    key: &str,
    model: &str,
    body: &Value,
) -> Result<CachedRun, String> {
    if let Some(run) = cached(key) {
        return Ok(run);
    }
    let session_id = create_session(client, sid).await?;
    let run_id = create_run(client, sid, &session_id, model, &last_user_text(body)).await?;
    let run = CachedRun { session_id, run_id, at: now_ms() };
    remember(key, &run);
    Ok(run)
}

async fn create_session(client: &reqwest::Client, sid: &str) -> Result<String, String> {
    let payload = http::post_json(
        client,
        sid,
        constants::SESSIONS_PATH,
        &json!({
            "session_type": "cowork_client",
            "agent_version": 2,
            "env": {
                "device_id": "agent2api-plugin",
                "device_name": "Agent2API plugin",
                "work_space_dir": "/tmp",
            },
        }),
        &[],
    )
    .await?;
    let session_id = http::data_of(&payload)
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if session_id.is_empty() {
        return Err("灵犀未返回 session_id".to_string());
    }
    Ok(session_id)
}

async fn create_run(
    client: &reqwest::Client,
    sid: &str,
    session_id: &str,
    model: &str,
    title: &str,
) -> Result<String, String> {
    let payload = http::post_json(
        client,
        sid,
        constants::RUNS_PATH,
        &json!({
            "client_request_id": now_ms().to_string(),
            "session_id": session_id,
            "model_provider": "lingxi",
            "model_id": model,
            "agent_input": {
                "text": title,
                "files": [],
                "file_ids": [],
                "expect_model": model,
                "refs": [],
                "permission_mode": "default",
                "current_mode": "agent",
                "browser_plugin_enabled": false,
            },
        }),
        &[],
    )
    .await?;
    let run_id = http::data_of(&payload)
        .get("run_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if run_id.is_empty() {
        return Err("灵犀未返回 run_id".to_string());
    }
    Ok(run_id)
}

async fn forward(
    client: &reqwest::Client,
    sid: &str,
    run: &CachedRun,
    model: &str,
    body: &Value,
    sink: &mut ConversationSink<'_>,
) -> Result<(), Failure> {
    let request = build_body(body, model).map_err(Failure::other)?;
    let extra = [
        ("x-cc-session-id", run.session_id.clone()),
        ("x-cc-run-id", run.run_id.clone()),
    ];
    let response = http::post_stream(client, sid, constants::CHAT_PATH, &request, &extra)
        .await
        .map_err(|message| {
            if looks_like_stale_run(&message) {
                Failure::run(message)
            } else {
                Failure::other(message)
            }
        })?;

    sink.head(proto::StreamHead {
        status: 200,
        headers: [("content-type".to_string(), "text/event-stream".to_string())].into_iter().collect(),
        upstream: proto::UpstreamKind::Chat,
    })
    .await
    .map_err(Failure::other)?;

    let mut translator = Translator::new(model);
    let mut stream = response.bytes_stream();
    use futures::StreamExt;
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|error| Failure::other(format!("读取灵犀流失败：{error}")))?;
        for frame in translator.push(&bytes).map_err(Failure::other)? {
            sink.send(&frame).await.map_err(Failure::other)?;
        }
    }
    for frame in translator.finish().map_err(Failure::other)? {
        sink.send(&frame).await.map_err(Failure::other)?;
    }
    Ok(())
}

/// 客户端 body → 灵犀直连口请求体。
fn build_body(body: &Value, model: &str) -> Result<Value, String> {
    let messages = body
        .get("messages")
        .cloned()
        .ok_or_else(|| "请求缺少 messages".to_string())?;
    if !messages.is_array() {
        return Err("messages 必须是数组".to_string());
    }

    let (tools, dropped) = filter_tools(body.get("tools"));
    if dropped > 0 {
        crate::logging::log(
            "[Lingxi]",
            &format!("丢弃 {dropped} 个非 function 工具（上游只认标准 OpenAI 形状）"),
        );
    }

    let mut out = Map::new();
    out.insert("stream".to_string(), Value::Bool(true));
    out.insert("model".to_string(), Value::String(model.to_string()));
    out.insert("messages".to_string(), messages);
    if !tools.is_empty() {
        out.insert("tools".to_string(), Value::Array(tools));
        if let Some(choice) = body.get("tool_choice").filter(|value| !value.is_null()) {
            out.insert("tool_choice".to_string(), choice.clone());
        }
    }
    if let Some(temperature) = body.get("temperature").filter(|value| value.is_number()) {
        out.insert("temperature".to_string(), temperature.clone());
    }
    if let Some(max_tokens) = body.get("max_tokens").and_then(Value::as_i64).filter(|value| *value > 0) {
        out.insert("base_llm_arguments".to_string(), json!({ "max_tokens": max_tokens }));
    }
    if let Some(effort) = body
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        out.insert("reasoning_effort".to_string(), Value::String(effort.to_string()));
        out.insert("llm_arguments".to_string(), json!({ "reasoning_effort": effort }));
    }
    Ok(Value::Object(out))
}

/// 只留 `{type:"function", function:{name}}`；返回（保留, 丢弃数）。
fn filter_tools(value: Option<&Value>) -> (Vec<Value>, usize) {
    let Some(Value::Array(items)) = value else {
        return (Vec::new(), 0);
    };
    let mut kept = Vec::new();
    let mut dropped = 0;
    for item in items {
        let function = item.get("type").and_then(Value::as_str) == Some("function")
            && item
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .is_some_and(|name| !name.trim().is_empty());
        if function {
            kept.push(item.clone());
        } else {
            dropped += 1;
        }
    }
    (kept, dropped)
}

fn last_user_text(body: &Value) -> String {
    let text = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        })
        .map(|message| match message.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<&str>>()
                .join(" "),
            _ => String::new(),
        })
        .unwrap_or_default();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "灵犀会话".to_string()
    } else {
        trimmed.chars().take(60).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_carries_tools_and_drops_others() {
        let body = json!({
            "model": "lingxi/GLM-5.3",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                { "type": "function", "function": { "name": "get_weather" } },
                { "type": "web_search" }
            ],
            "tool_choice": "auto",
            "max_tokens": 1024,
            "reasoning_effort": "high"
        });
        let built = build_body(&body, "GLM-5.3").expect("build");
        assert_eq!(built["stream"], true);
        assert_eq!(built["model"], "GLM-5.3");
        assert_eq!(built["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(built["tool_choice"], "auto");
        assert_eq!(built["base_llm_arguments"]["max_tokens"], 1024);
        assert_eq!(built["llm_arguments"]["reasoning_effort"], "high");
    }

    #[test]
    fn no_tools_means_no_tool_choice() {
        let body = json!({
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [{ "type": "web_search" }],
            "tool_choice": "auto"
        });
        let built = build_body(&body, "GLM-5.3").expect("build");
        assert!(built.get("tools").is_none());
        assert!(built.get("tool_choice").is_none());
    }

    #[test]
    fn stale_run_messages_are_detected() {
        assert!(looks_like_stale_run("灵犀上游返回 SQLNoRows 数据不存在"));
        assert!(looks_like_stale_run("灵犀上游返回错误：数据不存在"));
        assert!(!looks_like_stale_run("灵犀上游返回 429：限流"));
    }

    #[test]
    fn title_is_trimmed_from_last_user_message() {
        let body = json!({ "messages": [
            { "role": "user", "content": "old" },
            { "role": "assistant", "content": "ok" },
            { "role": "user", "content": "x".repeat(200) }
        ]});
        assert_eq!(last_user_text(&body).chars().count(), 60);
    }
}
