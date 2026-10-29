//! Command Code 模型目录：`GET /provider/v1/models` + 5 分钟缓存 + 内置兜底清单。
//!
//! ── 上游形态 ────────────────────────────────────────────────
//! ```text
//!   { "data": [ { "id": "deepseek/deepseek-v4-flash", "object": "model", … } ] }
//! ```
//! 只依赖 `data[].id`，并且**失败时回落到内置清单**（26 项）—— 绝不把目录
//! 清空（清了会让「这家有账号但模型列表是空的」）。
//!
//! ── 模型 id 的命名规律 ──────────────────────────────────────
//! Anthropic / OpenAI 系**无前缀**，开源系**带厂商前缀**。id 原样透传：
//! 本家不做任何名称重写。
//!
//! ── 能力位的来源（为什么只有图片位是「点名给」的）─────────────────
//! 上游目录**不返回**结构化能力位。工具调用是本家 wire 协议的一等公民
//! （`params.tools` 总是下发）→ true；思考同样是一等字段 → true。
//! 图片位**没有目录依据**，只有规格点名的几支给 true，其余 conservative
//! false —— 报错方向只能选「少报」。
//!
//! ── 缓存为什么不落盘（与网关版的一处差别）──────────────────────
//! 网关版把目录写进 `catalog_cache`（进程重启后读回）。插件侧没有那个设施，
//! 而且**不需要**：网关自己会缓存插件返回的清单（`rpc_cache`），插件进程
//! 重启后重新拉一次即可。少一层落盘就少一处「插件和网关的缓存不一致」。

use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use agent2api_provider_protocol as proto;

use super::endpoints;
use crate::http;

/// 自动路径的缓存 TTL
const TTL_MS: i64 = 5 * 60 * 1000;

/// 内置兜底清单（id + 展示名）。
///
/// 上游拉不到时的保底：它同时是「没有账号时点什么」的那张表。
/// id 是转发时直接下发的名字，改一个字母就等于换模型，逐字保留。
const FALLBACK_MODELS: [(&str, &str); 26] = [
    ("claude-sonnet-4-6", "Claude Sonnet 4.6"),
    ("claude-opus-4-8", "Claude Opus 4.8"),
    ("claude-opus-4-7", "Claude Opus 4.7"),
    ("claude-haiku-4-5-20251001", "Claude Haiku 4.5"),
    ("gpt-5.5", "GPT-5.5"),
    ("gpt-5.4", "GPT-5.4"),
    ("gpt-5.4-mini", "GPT-5.4 Mini"),
    ("gpt-5.3-codex", "GPT-5.3 Codex"),
    ("deepseek/deepseek-v4-pro", "DeepSeek V4 Pro"),
    ("deepseek/deepseek-v4-flash", "DeepSeek V4 Flash"),
    ("moonshotai/Kimi-K2.6", "Kimi K2.6"),
    ("moonshotai/Kimi-K2.5", "Kimi K2.5"),
    ("zai-org/GLM-5.1", "GLM 5.1"),
    ("zai-org/GLM-5", "GLM 5"),
    ("MiniMaxAI/MiniMax-M3", "MiniMax M3"),
    ("MiniMaxAI/MiniMax-M2.7", "MiniMax M2.7"),
    ("MiniMaxAI/MiniMax-M2.5", "MiniMax M2.5"),
    ("Qwen/Qwen3.6-Max-Preview", "Qwen 3.6 Max Preview"),
    ("Qwen/Qwen3.6-Plus", "Qwen 3.6 Plus"),
    ("Qwen/Qwen3.7-Max", "Qwen 3.7 Max"),
    ("stepfun/Step-3.7-Flash", "Step 3.7 Flash"),
    ("stepfun/Step-3.5-Flash", "Step 3.5 Flash"),
    ("xiaomi/mimo-v2.5-pro", "MiMo V2.5 Pro"),
    ("xiaomi/mimo-v2.5", "MiMo V2.5"),
    ("google/gemini-3.5-flash", "Gemini 3.5 Flash"),
    ("google/gemini-3.1-flash-lite", "Gemini 3.1 Flash Lite"),
];

/// 进程内缓存
struct CatalogState {
    models: Vec<Value>,
    fetched_at: i64,
}

static STATE: OnceLock<Mutex<Option<CatalogState>>> = OnceLock::new();

/// 取状态句柄（毒锁按「继续用」处理：这份缓存是可重建的展示数据）
fn state() -> &'static Mutex<Option<CatalogState>> {
    STATE.get_or_init(|| Mutex::new(None))
}

/// 在锁内执行一段取值
fn with_state<R>(f: impl FnOnce(&mut Option<CatalogState>) -> R) -> R {
    let mutex = state();
    let mut guard = mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

/// 这个模型 id 是否声明支持图片输入（**只有规格点名的几支**）
fn supports_images(id: &str) -> bool {
    let lowered = id.to_lowercase();
    lowered.contains("vision") || lowered.contains("mimo-v2.5") || lowered.contains("kimi-k2.5")
}

/// 把一个上游目录条目映射成聚合层认的形态
fn map_entry(row: &Value) -> Option<Value> {
    let id = row
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let name = row
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(id);
    Some(json!({
        "id": id,
        "name": name,
        "supportsToolCall": true,
        "supportsReasoning": true,
        "supportsImages": supports_images(id),
    }))
}

/// 内置兜底清单的聚合层形态
pub fn fallback_models() -> Vec<Value> {
    FALLBACK_MODELS
        .iter()
        .map(|(id, name)| {
            json!({
                "id": id,
                "name": name,
                "supportsToolCall": true,
                "supportsReasoning": true,
                "supportsImages": supports_images(id),
            })
        })
        .collect()
}

/// 解析 `/provider/v1/models` 的响应（`{data:[…]}` 或裸数组）
pub fn parse_models(payload: &Value) -> Vec<Value> {
    let rows = payload
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| payload.as_array())
        .cloned()
        .unwrap_or_default();
    rows.iter().filter_map(map_entry).collect()
}

/// 当前模型清单（内存 → 内置兜底；永远非空）
pub fn list() -> proto::ModelList {
    with_state(|slot| match slot.as_ref() {
        Some(state) if !state.models.is_empty() => proto::ModelList {
            models: state.models.clone(),
            meta: proto::CatalogMeta {
                remote: true,
                refreshed_at: state.fetched_at,
            },
        },
        // 没有远程清单 = 内置兜底（`remote: false` 让界面显示「内置」）
        _ => proto::ModelList {
            models: fallback_models(),
            meta: proto::CatalogMeta::default(),
        },
    })
}

/// 拉一次模型目录并落地（内存缓存）。
///
/// `force = false`（自动路径）时 5 分钟 TTL 内早退为 `unchanged()`；
/// `force = true`（用户手动刷新）真打上游。
///
/// 失败时**保留现有清单**，返回 `failed(原因)` 让手动路径如实汇报。
pub async fn refresh(api_key: &str, force: bool, gateway_proxy: &str) -> proto::RefreshOutcome {
    let now = http::now_ms();
    if !force {
        let fresh = with_state(|slot| {
            slot.as_ref()
                .map(|state| !state.models.is_empty() && now - state.fetched_at < TTL_MS)
                .unwrap_or(false)
        });
        if fresh {
            return proto::RefreshOutcome::unchanged();
        }
    }
    let mut headers = endpoints::cli_headers();
    headers.extend(endpoints::auth_headers(api_key));
    let client = http::control_client(None, gateway_proxy);
    let (status, payload) = match http::get_json(
        &client,
        &endpoints::url(endpoints::MODELS_PATH),
        &headers,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            return proto::RefreshOutcome::failed(format!(
                "Command Code 模型目录查询失败：{error}（保留现有清单）"
            ))
        }
    };
    if !(200..300).contains(&status) {
        return proto::RefreshOutcome::failed(format!(
            "Command Code 模型目录返回 HTTP {status}（保留现有清单）"
        ));
    }
    let models = parse_models(&payload);
    if models.is_empty() {
        return proto::RefreshOutcome::failed(
            "Command Code 模型目录为空或格式不认识（保留现有清单）",
        );
    }
    let count = models.len();
    with_state(|slot| {
        *slot = Some(CatalogState {
            models: models.clone(),
            fetched_at: now,
        });
    });
    proto::RefreshOutcome::refreshed(count)
}
