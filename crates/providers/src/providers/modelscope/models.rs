//! ModelScope 模型目录：`GET {推理域}/models`（标准 OpenAI 形态）+ 进程缓存。
//!
//! ── 上游形态（真实账号实测）────────────────────────────────
//! ```text
//! GET https://api-inference.modelscope.cn/v1/models     Authorization: Bearer <ms-令牌>
//!   → {"object":"list","data":[{"id":"Qwen/Qwen3.8-Flash-Next",
//!                               "object":"","owned_by":"system","created":1789…}, …]}
//! ```
//! **只有 `id`**：没有 `name`、没有 `capabilities`、没有上下文长度。
//!
//! ── 清单是**账号相关**的（这是不做静态兜底表的理由）────────────
//! 实测同一个令牌拉到的是 **36 条**，而不是魔搭全站的几千个模型 ——
//! 它给的是「这个账号在 API-Inference 上能调的模型」。编一份静态兜底表是
//! **有害的**：用户会看到一批自己其实调不通的模型。拉不到就如实报失败、
//! 保留旧清单。
//!
//! ── 清单里混着非对话模型（照实列出，不筛）────────────────────
//! 实测 36 条里含生图（`Qwen/Qwen-Image-Edit`）、基座（`PaddlePaddle/ERNIE-*-PT`）
//! 等并非 chat 用途的条目。上游给的就是它的清单，**不按 id 猜用途去过滤** ——
//! 猜错会把能用的模型藏起来。真调不通时，`classify` 会把上游的 400 如实透出。
//!
//! ── 两个地区两套缓存 ────────────────────────────────────────
//! 两个站的清单互不覆盖 —— 这正是当初把地区拆成两家 provider 要解决的问题。

use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use agent2api_provider_protocol as proto;

use super::client;
use super::region::Region;
use crate::http;

/// 模型列表路径（相对推理域）
pub const MODELS_PATH: &str = "/models";

/// 自动路径的缓存 TTL（与各家同档）
const TTL_MS: i64 = 10 * 60 * 1000;

/// 进程内缓存
struct CatalogState {
    models: Vec<Value>,
    fetched_at: i64,
}

/// 两个地区各一个槽
static STATE_CN: OnceLock<Mutex<Option<CatalogState>>> = OnceLock::new();
static STATE_INTL: OnceLock<Mutex<Option<CatalogState>>> = OnceLock::new();

/// 取本地区的状态句柄（毒锁按「继续用」处理：这份缓存是可重建的展示数据）
fn state(region: Region) -> &'static Mutex<Option<CatalogState>> {
    match region {
        Region::Cn => STATE_CN.get_or_init(|| Mutex::new(None)),
        Region::Intl => STATE_INTL.get_or_init(|| Mutex::new(None)),
    }
}

/// 在锁内执行一段取值
fn with_state<R>(region: Region, f: impl FnOnce(&mut Option<CatalogState>) -> R) -> R {
    let mutex = state(region);
    let mut guard = mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

/// 把一个 OpenAI 目录条目映射成聚合层认的形态。
///
/// 上游只给 `id`，所以 `name` 就用 id 本身：魔搭的 id 是 `组织/模型名`，
/// 它本身就是可读的展示名，拆掉组织段反而会让同名模型（不同组织的同名条目）
/// 在界面上无法区分。
///
/// 工具位按 OpenAI 兼容网关的既有能力给 true。**思考位不给** —— 上游没有
/// 能力位、按 id 猜会猜错，交给模型规则层由用户显式绑定更诚实。
fn map_entry(row: &Value) -> Option<Value> {
    let id = row
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let mut model = json!({
        "id": id,
        "name": id,
        "supportsToolCall": true,
    });
    super::image_generation::decorate(id, &mut model);
    Some(model)
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

/// 本地区当前的模型清单（内存缓存；没有时为空，由刷新路径填）
pub fn list(region: Region) -> proto::ModelList {
    with_state(region, |slot| match slot.as_ref() {
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

/// 拉一次模型目录并落地（内存缓存）。
///
/// `force = false`（自动路径）时 10 分钟 TTL 内早退为 `unchanged()`；
/// `force = true`（用户手动刷新）真打上游。
pub async fn refresh(
    region: Region,
    api: &client::ControlClient,
    force: bool,
) -> proto::RefreshOutcome {
    let now = http::now_ms();
    if !force {
        let fresh = with_state(region, |slot| {
            slot.as_ref()
                .map(|state| !state.models.is_empty() && now - state.fetched_at < TTL_MS)
                .unwrap_or(false)
        });
        if fresh {
            return proto::RefreshOutcome::unchanged();
        }
    }
    let payload = match api.models_json(MODELS_PATH).await {
        Ok(payload) => payload,
        Err(error) => return proto::RefreshOutcome::failed(error),
    };
    let models = parse_models(&payload);
    if models.is_empty() {
        return proto::RefreshOutcome::failed(
            "ModelScope 模型目录为空或格式不认识（保留现有清单）",
        );
    }
    let count = models.len();
    with_state(region, |slot| {
        *slot = Some(CatalogState {
            models: models.clone(),
            fetched_at: now,
        });
    });
    proto::RefreshOutcome::refreshed(count)
}
