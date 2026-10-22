//! LobsterAI 的模型目录：`GET /api/models/available` 是权威来源，**另有一份
//! 19 条静态兜底**（远程拉不到 / 关掉发现时用）。
//!
//! ── 响应信封：`data` 是**单层数组**，别拿通用对象信封解析器来套 ──────
//! ```text
//!   {"code":0,"message":"success","data":[ {modelId, modelName, …}, … ]}
//! ```
//! Go 参考在 `models.go:216-244` 明确警告：它**不**复用 `parseEnvelope` ——
//! 那个解析器要求 `data` 是**对象**（这样 `data:null` 才能被当成「令牌失效」），
//! 而这个端点的 `data` 是数组；套上去会让整份模型清单**静默为空**。
//! 本模块自带 [`read_model_array`]，并兼容「双层层套」（`data.data[]`，
//! 参考实现说野生形态里两种都出现过）。
//!
//! ── keyfrom 只进 query，**绝不带 refresh token** ────────────────
//! 目录端点的身份是 `firstKeyfrom` / `latestKeyfrom` / `version`（+ 可选的
//! `uuid` / `userId`），不带 refresh token —— query 会进服务端的访问日志
//! （`models.go:523-526` 的原话）。拼法与 `credentials::keyfrom_fields` 同源。
//!
//! ── 两个 `X-LobsterAI-Client-*` 头**在这条端点上尤其重要** ─────────
//! 服务端按 `ClientCapabilities` **过滤**返回的模型集合：不带
//! `kimi-k3-agentic-v1` 时目录里少一个模型（实测 25 vs 26，见
//! `endpoints.rs` 的模块头）。少发这个头不是「少一条头」，是**少一个模型**。
//!
//! ── 字段只按上游声明的取值，缺了就不广告 ─────────────────────────
//! `supportsImage` / `supportsThinking` / `costMultiplier` 缺失时**保持未知**
//! （`0` 与「字段不存在」是两回事：`0` 意思是免费）—— 与 Go 的 `remoteModel`
//! 注释（`models.go:110-131`）同一口径：编一个 `false` / `0` 会把将来的模型
//! 静默归类错。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::sync::{Mutex, OnceLock};

use serde_json::{Map, Value};

use agent2api_provider_protocol as proto;

use crate::errors::GatewayError;
use crate::http::send_raw;
use crate::logging;

use super::chat;
use super::credentials::LobsterCredentials;
use super::endpoints::{
    CATALOG_TIMEOUT_MS, CLIENT_CAPABILITIES, MODELS_PATH, USER_AGENT, api_base, models_url,
    urlencode,
};

/// 目录缓存时长（与参考实现的 2 小时同档）
const CATALOG_TTL_MS: i64 = 2 * 60 * 60 * 1000;

/// 兜底目录里每条模型的上下文窗口（**照抄参考实现**，是桥接层的估计值
/// `131072`，**不是**逐模型实测 —— 见 [`fallback_models`] 的说明）
const FALLBACK_CONTEXT_WINDOW: i64 = 131_072;

/// 目录里的一条模型
#[derive(Clone, Debug)]
pub struct ModelEntry {
    /// 发给上游的模型名（`modelId`）—— 也是目录对外广告的 `id`
    pub wire_id: String,
    /// 展示名（`modelName`，缺时等于 `wire_id`）
    pub display_name: String,
    /// 上下文窗口（上游没给 = 0，不猜）
    pub context_window: i64,
    /// 单次输出上限（`maxTokens`；上游没给 = 0）
    pub max_tokens: i64,
    /// 是否收图片（上游没给 = None，**不猜**）
    pub supports_image: Option<bool>,
    /// 是否支持思考（`supportsThinking` 或 `thinkingConfig` 有档位）
    pub supports_thinking: bool,
    /// 积分倍率（**裸数字**，与腾讯系的 `"x0.05"` 字符串不同）。
    /// None = 字段缺失；Some(0.0) = 免费（**不能**当成缺失去掉）
    pub cost_multiplier: Option<f64>,
}

/// 进程内目录状态（**只装远程目录**；空表示还没拉到过，此时对外用兜底清单）
#[derive(Clone, Debug, Default)]
struct CatalogState {
    entries: Vec<ModelEntry>,
    fetched_at: i64,
}

static STATE: OnceLock<Mutex<Option<CatalogState>>> = OnceLock::new();

/// 取状态句柄（毒锁按「可重建的展示数据」处理，与本仓其它目录模块同口径）
fn state() -> &'static Mutex<Option<CatalogState>> {
    STATE.get_or_init(|| Mutex::new(None))
}

/// 在锁内执行一段取值。
///
/// ── 为什么不再从落盘缓存装回 ────────────────────────────────
/// 插件侧的目录缓存由**网关**负责持久化（`plugin_host::rpc_cache` + 目录库）：
/// 网关心跳重启后向插件要一次 `models.list`，插件给的清单就是它上次拉到的
/// 那一份。插件自己再落一份盘会与网关那份出现「两份真相」——
/// 用户改了网关的缓存策略而插件还按自己的 TTL 走，是最难查的那类不一致。
fn with_state<R>(f: impl FnOnce(&mut Option<CatalogState>) -> R) -> R {
    let mut guard = match state().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(&mut guard)
}

/// 静态兜底目录：**19 条，逐条照抄 Go 参考**（`models.go:138-158` 的
/// `fallbackModels`，其顺序也照抄，便于与上游对账）。上下文窗口统一
/// [`FALLBACK_CONTEXT_WINDOW`]（那是桥接层的估计值，不是逐模型实测）。
///
/// 只在「远程发现关掉 / 拉取失败」时对外 —— 远程一旦成功就是权威。
fn fallback_models() -> Vec<ModelEntry> {
    const IDS: [&str; 19] = [
        "deepseek-v4-flash",
        "deepseek-v4-pro",
        "MiniMax-M3",
        "MiniMax-M2.7",
        "qwen3.7-max",
        "qwen3.7-plus",
        "qwen3.6-plus",
        "qwen3.5-plus-2026-04-20",
        "kimi-k2.7-code",
        "kimi-k2.7-code-highspeed",
        "kimi-k2.6",
        "kimi-k2.5",
        "doubao-seed-2-1-pro-260628",
        "doubao-seed-2-1-turbo-260628",
        "doubao-seed-2-0-code-preview-260215",
        "glm-5.2",
        "glm-5.1",
        "glm-5v-turbo",
        "glm-5",
    ];
    IDS.iter()
        .map(|id| ModelEntry {
            wire_id: (*id).to_string(),
            display_name: (*id).to_string(),
            context_window: FALLBACK_CONTEXT_WINDOW,
            max_tokens: 0,
            supports_image: None,
            supports_thinking: false,
            // 兜底清单里**不带**倍率：那要上游逐条声明，编一个 0（免费）会误导
            cost_multiplier: None,
        })
        .collect()
}

/// 读一个数字（数字或数字字符串都认）
fn read_number(value: &Value) -> Option<f64> {
    if let Some(number) = value.as_f64() {
        return Some(number);
    }
    value.as_str().and_then(|text| text.trim().parse::<f64>().ok())
}

/// 读一个正数（`> 0` 才算，否则视为缺失 —— 与 Go `contextWindow > 0` 同口径）
fn read_positive(value: Option<&Value>) -> i64 {
    value
        .and_then(read_number)
        .filter(|number| *number > 0.0)
        .map(|number| number as i64)
        .unwrap_or(0)
}

/// `thinkingConfig` 是否声明了可选档位（非空 `options` 且不止一个 `off`）。
///
/// Go 的 `parseThinkingConfig` 会丢弃整块结构不符的配置，并且「只有 off 一档」
/// 等于「没有可选档位」—— 这里只需要一个布尔，按同一口径折叠。
fn thinking_config_present(value: Option<&Value>) -> bool {
    let Some(options) = value
        .and_then(|config| config.get("options"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    options
        .iter()
        .filter(|option| {
            option
                .get("openclawLevel")
                .and_then(Value::as_str)
                .is_some_and(|level| !level.trim().is_empty())
        })
        .count()
        > 1
}

/// 把一条 `models/available` 条目映射成本模块的形态（缺 `modelId` 的丢掉）。
fn entry_from_upstream(value: &Value) -> Option<ModelEntry> {
    let wire_id = value
        .get("modelId")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if wire_id.is_empty() {
        return None;
    }
    let display_name = match value.get("modelName").and_then(Value::as_str) {
        Some(name) if !name.trim().is_empty() => name.trim().to_string(),
        _ => wire_id.clone(),
    };
    Some(ModelEntry {
        display_name,
        context_window: read_positive(value.get("contextWindow")),
        max_tokens: read_positive(value.get("maxTokens")),
        supports_image: value.get("supportsImage").and_then(Value::as_bool),
        supports_thinking: value
            .get("supportsThinking")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || thinking_config_present(value.get("thinkingConfig")),
        // costMultiplier 是**裸数字**；`0` 意思是免费，必须保留而不是当缺失去掉
        cost_multiplier: value.get("costMultiplier").and_then(read_number),
        wire_id,
    })
}

/// 倍率的展示串：`0` → 免费，其余 → `x<数字>`（前端 `formatCredits` 认 `x\d`）。
///
/// 这条口径照抄 Go 的 `displayNameFor`（`models.go:303-314`）：`0` 渲染成
/// 「免费」而不是「x0」。差别只在本仓把它放进 `credits` 键而不是拼进展示名。
fn credits_label(multiplier: f64) -> String {
    if multiplier == 0.0 {
        return "免费".to_string();
    }
    format!("x{multiplier}")
}

/// 聚合层认的条目形态（键名对照 `core/models/shape.rs::list_item`）。
///
/// `credits` 放**倍率展示串**（本仓的「倍率」列读的就是它，见 loomy 的同款处置）；
/// `0` 意思是免费。
fn to_manifest_value(entry: &ModelEntry) -> Value {
    let mut object = Map::new();
    object.insert("id".to_string(), Value::String(entry.wire_id.clone()));
    if !entry.display_name.is_empty() {
        object.insert("name".to_string(), Value::String(entry.display_name.clone()));
    }
    if entry.context_window > 0 {
        object.insert("maxInputTokens".to_string(), Value::from(entry.context_window));
    }
    if entry.max_tokens > 0 {
        object.insert("maxOutputTokens".to_string(), Value::from(entry.max_tokens));
    }
    // 模态位：上游**没给**时按不支持折叠，但不声称支持（有实测依据才广告）
    object.insert(
        "supportsImages".to_string(),
        Value::Bool(entry.supports_image.unwrap_or(false)),
    );
    object.insert("supportsVideo".to_string(), Value::Bool(false));
    object.insert(
        "supportsReasoning".to_string(),
        Value::Bool(entry.supports_thinking),
    );
    // 上游就是 OpenAI Chat Completions，tools 原样透传 ⇒ 支持工具调用
    object.insert("supportsToolCall".to_string(), Value::Bool(true));
    if let Some(multiplier) = entry.cost_multiplier {
        object.insert("credits".to_string(), Value::String(credits_label(multiplier)));
    }
    Value::Object(object)
}

/// 当前对外目录：**远程优先，没有则兜底**
pub fn list() -> proto::ModelList {
    proto::ModelList {
        models: raw_list(),
        meta: proto::CatalogMeta {
            remote: remote_refreshed(),
            refreshed_at: last_refreshed_at(),
        },
    }
}

/// 清单本体（`list()` 的内层，便于别处只要数组时不必拆 `ModelList`）。
pub fn raw_list() -> Vec<Value> {
    with_state(|guard| match guard.as_ref() {
        Some(state) if !state.entries.is_empty() => {
            state.entries.iter().map(to_manifest_value).collect()
        }
        _ => fallback_models().iter().map(to_manifest_value).collect(),
    })
}

/// 目录里有没有**远程拉到的**条目（`false` 时 [`list`] 给的是兜底清单）
pub fn remote_refreshed() -> bool {
    with_state(|guard| {
        guard
            .as_ref()
            .map(|state| state.fetched_at > 0 && !state.entries.is_empty())
            .unwrap_or(false)
    })
}

/// 目录最后一次落地时刻（毫秒；0 = 没有远程目录）
pub fn last_refreshed_at() -> i64 {
    with_state(|guard| guard.as_ref().map(|state| state.fetched_at).unwrap_or(0))
}

/// 当前远程目录的年龄（毫秒；0 = 没有远程目录）
pub fn age_ms() -> i64 {
    with_state(|guard| {
        guard
            .as_ref()
            .map(|state| logging::now_ms() - state.fetched_at)
            .unwrap_or(0)
    })
}

/// Go 参考的**规范化别名 → 线上 id** 表（`models.go:388-395` 的 `upstreamModelIDs`）。
///
/// ── 为什么需要它（这条回答「发布名能不能不等于线上 id」）────────────
/// **能**。Go 参考对外发布的是**规范化大小写的 id**（`publicModelIDs`：
/// `deepseek-flash` → `DeepSeek-V4.1-Flash`），并另建一张反向表把请求里的
/// 规范化名折回线上 id（它自己的注释说：CPA 会跳过只差大小写的别名，所以规范
/// 大小写必须由插件自己发布）。本仓的目录直接以 `modelId`（线上 id）作为对外
/// `id`，因此**绝大多数情况下两者相同**；这张表只为「客户端手里握着旧规范化名」
/// 那条路兜底。
///
/// 键一律按小写比较。
const UPSTREAM_ALIASES: [(&str, &str); 6] = [
    ("deepseek-v4.1-flash", "deepseek-flash"),
    ("glm-5.3-flash", "glm-5.3-flash"),
    ("glm-5.3", "glm-5.3"),
    ("qwen3.8-max", "qwen3.8-max"),
    ("qwen3.8-flash-next", "qwen3.8-flash"),
    ("kimi-k2.7-code", "kimi-k2.7-code"),
];

/// 客户端请求里的模型名 → 发给上游的模型名。
///
/// 顺序：远程目录（按线上 id 或展示名命中）→ 规范化别名表 → 认不出返回 `None`
/// （由调用方**原样发**：猜一个更接近的名字比原样发更糟 —— 那会把「名字没映射」
/// 变成一个静默的错误重试）。
pub fn upstream_id_for(requested: &str) -> Option<String> {
    let target = requested.trim();
    if target.is_empty() {
        return None;
    }
    if let Some(wire_id) = with_state(|guard| {
        guard.as_ref().and_then(|state| {
            state
                .entries
                .iter()
                .find(|entry| entry.wire_id == target || entry.display_name == target)
                .map(|entry| entry.wire_id.clone())
        })
    }) {
        return Some(wire_id);
    }
    let lower = target.to_lowercase();
    UPSTREAM_ALIASES
        .iter()
        .find(|(alias, _)| *alias == lower)
        .map(|(_, wire)| (*wire).to_string())
}

/// 把 `keyfrom_fields` 拼成 URL query（`k=v&k2=v2`，值 urlencoded，**不含
/// refresh token** —— 见模块头）。
fn keyfrom_query(credentials: &LobsterCredentials, client_version: &str) -> String {
    let fields = credentials.keyfrom_fields(client_version);
    let mut query = String::new();
    for (key, value) in fields {
        let escaped = urlencode(&value);
        if !query.is_empty() {
            query.push('&');
        }
        query.push_str(key);
        query.push('=');
        query.push_str(&escaped);
    }
    query
}

/// 从响应里取模型数组。
///
/// **不**复用通用的对象信封解析器（会要求 `data` 是对象，从而把整份清单判空，
/// 见模块头）；`code != 0` 一律判失败，`data` 支持单层数组与 `data.data[]` 双层。
pub fn read_model_array(payload: &Value) -> Option<Vec<Value>> {
    let record = payload.as_object()?;
    let code = record.get("code").and_then(read_number)?;
    if code != 0.0 {
        return None;
    }
    if let Some(array) = record.get("data").and_then(Value::as_array) {
        return Some(array.clone());
    }
    record
        .get("data")
        .and_then(Value::as_object)
        .and_then(|nested| nested.get("data"))
        .and_then(Value::as_array)
        .cloned()
}

/// 目录端点 URL（query 为空时退回不带 `?` 的形态，与 Go `buildModelsURL` 同口径）
fn catalog_url(query: &str) -> String {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        format!("{}{}", api_base(), MODELS_PATH)
    } else {
        models_url(trimmed)
    }
}

/// 拉一次目录（返回解析后的条目；`code != 0` 或形状不符时报错）。
async fn fetch_catalog(
    credentials: &LobsterCredentials,
    client_version: &str,
) -> Result<Vec<ModelEntry>, GatewayError> {
    // 版本头与 query 里的 version 用**同一个值**（都由 `chat::prime_client_version`
    // 给出），避免「声明是这个版本、目录按那个版本过滤」的错位。
    let version = if client_version.trim().is_empty() {
        chat::client_version_header()
    } else {
        client_version.trim().to_string()
    };
    let url = catalog_url(&keyfrom_query(credentials, &version));
    let headers: Vec<(String, String)> = vec![
        (
            "authorization".to_string(),
            format!("Bearer {}", credentials.access_token),
        ),
        ("accept".to_string(), "application/json".to_string()),
        (
            "x-lobsterai-client-capabilities".to_string(),
            CLIENT_CAPABILITIES.to_string(),
        ),
        (
            "x-lobsterai-client-version".to_string(),
            version.clone(),
        ),
        ("user-agent".to_string(), USER_AGENT.to_string()),
    ];
    let response = send_raw("GET", &url, None, &headers, None, Some(CATALOG_TIMEOUT_MS))
        .await
        .map_err(|error| {
            if error.is_timeout() {
                GatewayError::with_status(504, "LobsterAI 目录接口超时")
            } else {
                GatewayError::with_status(502, format!("LobsterAI 目录接口请求失败: {error}"))
            }
        })?;
    let payload = response.payload.ok_or_else(|| {
        GatewayError::with_status(502, "LobsterAI 目录接口响应不是 JSON")
    })?;
    if !response.ok {
        return Err(GatewayError::with_status(
            response.status as i32,
            format!("LobsterAI 目录接口返回 HTTP {}", response.status),
        ));
    }
    let items = read_model_array(&payload).ok_or_else(|| {
        GatewayError::with_status(502, "LobsterAI 目录响应结构不符（code != 0 或缺 data 数组）")
    })?;
    Ok(items.iter().filter_map(entry_from_upstream).collect())
}

/// 刷新目录：远程成功即权威；失败时**保留现有清单**并如实汇报原因。
pub async fn refresh(credentials: &LobsterCredentials, force: bool) -> proto::RefreshOutcome {
    if !force && remote_refreshed() && age_ms() < CATALOG_TTL_MS {
        return proto::RefreshOutcome::unchanged();
    }

    // 顺手把客户端版本解析并写进快照（转发路径的同步版本头要用，见 `chat.rs`）
    let version = chat::prime_client_version().await;

    match fetch_catalog(credentials, &version).await {
        Ok(entries) if !entries.is_empty() => {
            let fetched_at = logging::now_ms();
            let count = entries.len();
            with_state(|guard| {
                *guard = Some(CatalogState {
                    entries,
                    fetched_at,
                });
            });
            logging::log(
                "[LobsterAI]",
                &format!("目录刷新完成：{count} 个模型（客户端版本 {version}）"),
            );
            proto::RefreshOutcome::refreshed(count)
        }
        Ok(_) => proto::RefreshOutcome::failed("LobsterAI 目录为空（上游未返回可用模型）"),
        Err(error) => {
            logging::verbose(
                "[LobsterAI]",
                &format!("目录刷新失败，沿用现有清单：{}", error.message),
            );
            proto::RefreshOutcome::failed(error.message)
        }
    }
}
