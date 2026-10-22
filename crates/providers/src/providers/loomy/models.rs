//! Loomy 模型目录：`GET {集成网关}/api/v1/models`（OpenAI 格式）+ 进程内缓存。
//!
//! ── 上游形态（客户端 `electron/model-service.js` 的 `fetchModelsFromProvider`）──
//! 标准 OpenAI 形态 `{data:[{id, ...}]}`；鉴权是**双头**：`token: <session>` 与
//! `Authorization: Bearer <session>`。
//!
//! 拉取失败时**不编造模型名**：Loomy 官方 imodel 网关没有内置兜底清单
//! （客户端 `BUILTIN_FALLBACK_MODELS` 为空表），这里如实报失败、保留旧清单。
//!
//! ── 缓存 ────────────────────────────────────────────────────
//! 只有进程内存（自动路径 10 分钟 TTL；用户手动「刷新模型清单」时 `force`
//! 绕过）。**不落盘** —— 网关自己会缓存插件返回的清单（`rpc_cache` +
//! `catalog_cache`），插件再存一份就是两套状态互相不一致。
//!
//! ── 能力位 ──────────────────────────────────────────────────
//! 思考 / 工具 / 图片 / 视频位都读上游条目的 `capabilities`（`reasoning` /
//! `function_calling` / `vision` / `input_modalities`，实测 2026-10 每个条目都
//! 带这四个键）。旧实现只按模型 id 判思考位（spark-x）、硬编码工具位 ——
//! 那会丢掉 GLM-5.3-Flash 等模型的 vision 声明，目录里全部显示成不支持图片。

use std::sync::{Mutex, OnceLock};

use agent2api_provider_protocol as proto;
use serde_json::{json, Value};

use super::client;
use crate::http;

/// 自动路径的缓存 TTL（与客户端自己的轮询周期同档）
const TTL_MS: i64 = 10 * 60 * 1000;

/// 模型列表路径（相对集成网关）
pub const MODELS_PATH: &str = "/api/v1/models";

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
    let mut guard = mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

/// 该模型 id 是不是思考模型（客户端只对 spark-x 挂 reasoning 能力位）
fn is_reasoning_model(id: &str) -> bool {
    id.eq_ignore_ascii_case("spark-x") || id.to_ascii_lowercase().contains("spark-x")
}

/// 从上游展示名里拆出**积分倍率**后缀（`(x3.0)` / `(x0.8)` / `（x12）`）。
///
/// ── 为什么倍率要从名字里拆（有实测依据，不是猜）────────────────
/// 上游 `/models` **没有**结构化的倍率字段，它把倍率拼在展示名尾部 ——
/// 实测（2026-10，真实账号拉下来的 11 条目录）：
/// `DeepSeek V4 Flash 0731 (x3.0)`、`GLM 5.3 Flash(x0.8)`、`Spark X2.5 (x0.1)`。
/// 而本网关的「倍率」列读的是 `credits`，不拆的话那一列永远显示 `—`。
///
/// 客户端自己也这么拆（`splitModelDisplayName`：末尾括号内容作为展示标签，
/// ASCII 与全角括号都认），这里跟随同一口径。与客户端唯一的差别：**只把
/// `x<数字>` 形态的括号算倍率** —— 其余标签（如「限时免费」）本网关没有承载
/// 字段，原样留在名字里，不静默丢弃信息。
fn split_rate_suffix(name: &str) -> (String, Option<String>) {
    let trimmed = name.trim_end();
    let (body, inner) = if let Some(body) = trimmed.strip_suffix(')') {
        match body.rfind('(') {
            Some(open) => (&body[..open], body[open + 1..].trim()),
            None => return (name.to_string(), None),
        }
    } else if let Some(body) = trimmed.strip_suffix('）') {
        match body.rfind('（') {
            Some(open) => (&body[..open], body[open + '（'.len_utf8()..].trim()),
            None => return (name.to_string(), None),
        }
    } else {
        return (name.to_string(), None);
    };
    let Some(number) = inner.strip_prefix('x').or_else(|| inner.strip_prefix('X')) else {
        return (name.to_string(), None);
    };
    let number = number.trim();
    if number.is_empty()
        || !number.chars().any(|ch| ch.is_ascii_digit())
        || !number.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
    {
        return (name.to_string(), None);
    }
    let cleaned = body.trim_end();
    if cleaned.is_empty() {
        return (name.to_string(), Some(format!("x{number}")));
    }
    (cleaned.to_string(), Some(format!("x{number}")))
}

/// 从上游 `capabilities` 对象里取布尔位（缺失 / 非布尔一律 false）
fn caps_bool(caps: Option<&serde_json::Map<String, Value>>, key: &str) -> bool {
    caps.and_then(|caps| caps.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// `capabilities.input_modalities` 数组里有没有指定模态（大小写不敏感）
fn caps_modal(caps: Option<&serde_json::Map<String, Value>>, modal: &str) -> bool {
    caps.and_then(|caps| caps.get("input_modalities"))
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|value| {
                value.as_str().is_some_and(|text| text.eq_ignore_ascii_case(modal))
            })
        })
}

/// 把一个 OpenAI 目录条目映射成聚合层认的形态
/// （键名对照网关的 `core/models/shape.rs::list_item`）。
fn map_entry(row: &Value) -> Option<Value> {
    let id = row
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let raw_name = row
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(id);
    let (name, rate) = split_rate_suffix(raw_name);
    let caps = row.get("capabilities").and_then(Value::as_object);
    let tool_call = caps_bool(caps, "function_calling") || caps.is_none();
    let reasoning = caps_bool(caps, "reasoning") || is_reasoning_model(id);
    let mut item = json!({
        "id": id,
        "name": name,
        "supportsToolCall": tool_call,
        "supportsReasoning": reasoning,
    });
    if caps_bool(caps, "vision") || caps_modal(caps, "image") {
        if let Some(object) = item.as_object_mut() {
            object.insert("supportsImages".to_string(), Value::Bool(true));
        }
    }
    if caps_modal(caps, "video") {
        if let Some(object) = item.as_object_mut() {
            object.insert("supportsVideo".to_string(), Value::Bool(true));
        }
    }
    if let Some(rate) = rate {
        if let Some(object) = item.as_object_mut() {
            object.insert("credits".to_string(), Value::String(rate));
        }
    }
    // 上游给了上下文 / 输出上限就透传（键名取自客户端自己的规范化器
    // `normalizeFetchedModelMetadata` 读的那组，是上游字段名的一手依据）。
    if let Some(object) = item.as_object_mut() {
        let pick_number = |keys: &[&str]| -> Option<i64> {
            for key in keys {
                if let Some(value) = row.get(*key).and_then(Value::as_i64) {
                    if value > 0 {
                        return Some(value);
                    }
                }
            }
            None
        };
        if let Some(max_input) = pick_number(&[
            "context_length",
            "context_window",
            "input_token_limit",
            "max_input_tokens",
            "maxInputTokens",
        ]) {
            object.insert("maxInputTokens".to_string(), Value::from(max_input));
        }
        if let Some(max_output) = pick_number(&[
            "max_output_tokens",
            "max_completion_tokens",
            "max_tokens",
            "maxOutputTokens",
        ]) {
            object.insert("maxOutputTokens".to_string(), Value::from(max_output));
        }
    }
    Some(item)
}

/// 解析 `/models` 响应（OpenAI `{data:[...]}` 或裸数组）
pub fn parse_models(payload: &Value) -> Vec<Value> {
    let rows = payload
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| payload.as_array())
        .cloned()
        .unwrap_or_default();
    rows.iter().filter_map(map_entry).collect()
}

/// 当前模型清单（没有时为空，由刷新路径填）
pub fn list() -> Vec<Value> {
    with_state(|slot| slot.as_ref().map(|state| state.models.clone()).unwrap_or_default())
}

/// 当前清单 + 元数据。
pub fn catalog() -> proto::ModelList {
    with_state(|slot| match slot.as_ref() {
        Some(state) => proto::ModelList {
            models: state.models.clone(),
            meta: proto::CatalogMeta {
                // 本家没有内置兜底清单：有内容就等于远程拉到过
                remote: !state.models.is_empty(),
                refreshed_at: state.fetched_at,
            },
        },
        None => proto::ModelList::default(),
    })
}

/// 该模型 id 在上游目录里声明的图片输入能力。
///
/// `Some(true|false)` = 目录里有这条记录（能力位缺失按 `false` 读）；
/// `None` = 查不到（目录还没刷出来 / 模型已下线）。
pub fn declared_image_input(model: &str) -> Option<bool> {
    let wire = model.trim();
    if wire.is_empty() {
        return None;
    }
    list()
        .iter()
        .find(|item| {
            item.get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| id.eq_ignore_ascii_case(wire))
        })
        .map(|item| {
            item.get("supportsImages")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
}

/// 拉一次模型目录并落地（内存缓存）。
///
/// `force = false`（自动路径）时 10 分钟 TTL 内早退为 `unchanged()`；
/// `force = true`（用户手动刷新）真打上游。
pub async fn refresh(
    client: &reqwest::Client,
    session: &str,
    force: bool,
) -> proto::RefreshOutcome {
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
    let payload =
        match client::token_request(client, "GET", MODELS_PATH, session, None, "模型目录查询").await
        {
            Ok(payload) => payload,
            Err(error) => return proto::RefreshOutcome::failed(error),
        };
    let models = parse_models(&payload);
    if models.is_empty() {
        return proto::RefreshOutcome::failed("Loomy 模型目录为空或格式不认识（保留现有清单）");
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
