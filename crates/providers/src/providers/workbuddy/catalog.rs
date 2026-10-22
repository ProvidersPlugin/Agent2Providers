//! WorkBuddy 的模型目录刷新：`GET {endpoint}/v3/config` → `data.models`。
//!
//! ── 与网关那份的差别（迁移时改的）────────────────────────────
//! 网关走 `ModelCatalog::refresh_with_current_account`，那个类型还管**持久化
//! 缓存**（`catalog_cache`）、**启动加载**、**进程级句柄**与**默认规则种子**。
//! 插件这边这些都不该有：
//!
//! | 网关那一层 | 插件里归谁 |
//! |---|---|
//! | 进程级 `ModelCatalog` 句柄 | 网关的 `plugin_host::rpc_cache`（`models.refresh` 的结果由它持有） |
//! | `catalog_cache` 持久化 | 网关（插件重启后由网关重放缓存） |
//! | `model_rules::seed_workbuddy_defaults` | 网关（模型规则是**用户配置**，与插件无关） |
//! | 启动时自动刷新 | 网关的目录刷新排期（`catalog_refresh`） |
//!
//! 所以本文件只做一件事：**拿一份账号 → 打一次上游 → 解析 → 交回清单**。
//!
//! ── 账号选取三档（与网关同口径）──────────────────────────────
//!   · 点名了（`account` 非空）→ 按 id 直取，取不到即失败、**不回落到队首**
//!     （否则清单会变成「选了 A、用的是 B」）；
//!   · 没点名 → 本 provider 的队首账号；
//!   · 一条账号都没有 → 环境变量旁路（`WORKBUDDY[_INTL]_TOKEN`）。
//!
//! ── 企业账号的第二跳 ────────────────────────────────────────
//! `/v3/config` 的 `data.models` 为空、且账号带 `enterpriseId` 时，网关再打
//! `GET {base}/console/enterprises/{id}/config/models`。这条**保留**：企业版
//! 上游的模型清单确实挂在那条路径上（不是 `/v3/config` 的子集）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::Value;

use agent2api_provider_protocol::WireProxy;
use agent2api_provider_sdk::PluginCtx;

use crate::http::send_raw;
use crate::logging;
use crate::store;

use crate::workbuddy_shared::endpoints::{
    normalize_endpoint, user_agent_for_edition, WORKBUDDY_CLIENT_VERSION,
};

use super::auth;
use super::builtin;
use super::region::Region;

/// 目录请求超时（网关 `RefreshParams` 走的是 `send_raw` 的默认档，这里显式给 20 秒）。
const REFRESH_TIMEOUT_MS: u64 = 20_000;

/// 拿不到登录态时的原因文案（与网关 `REASON_NO_SESSION` 逐字相同）。
const REASON_NO_SESSION: &str = "缺少登录态，使用内置模型目录";

/// 一次目录刷新的结果（网关 `RefreshOutcome` 的插件侧等价物）。
///
/// 插件协议只要「成功几条 / 失败原因」，所以这里不带网关那个 `source` 字段 ——
/// 界面上「来源」那一列由网关按「有没有远程清单」自己算。
pub struct CatalogOutcome {
    /// 是否真拿到了可用清单。
    pub refreshed: bool,
    /// 拿到的清单（`refreshed == false` 时为空）。
    pub models: Vec<Value>,
    /// 没刷成的原因（`refreshed == true` 时为空）。
    pub reason: String,
}

impl CatalogOutcome {
    /// 成功：清单条数
    pub fn count(&self) -> usize {
        self.models.len()
    }

    fn ok(models: Vec<Value>) -> Self {
        Self {
            refreshed: true,
            models,
            reason: String::new(),
        }
    }

    fn failed(reason: impl Into<String>) -> Self {
        Self {
            refreshed: false,
            models: Vec::new(),
            reason: reason.into(),
        }
    }
}

/// 刷新某个地区的目录。
///
/// `account` 为空 = 用该地区的队首账号（见模块头的三档）。
pub async fn refresh(ctx: &PluginCtx, region: Region, account: &str) -> CatalogOutcome {
    let Some((session, proxy)) = pick_session(ctx, region, account).await else {
        logging::verbose("[Models]", &format!("模型目录未更新: {REASON_NO_SESSION}"));
        return CatalogOutcome::failed(REASON_NO_SESSION);
    };
    refresh_for_session(region, &session, proxy.as_ref()).await
}

/// 取一份会话 + 它的出口（三档选取，见模块头）。
async fn pick_session(
    ctx: &PluginCtx,
    region: Region,
    account: &str,
) -> Option<(Value, Option<WireProxy>)> {
    let requested = account.trim();
    if requested.is_empty() {
        // 队首账号
        if let Ok(record) = store::record(ctx, region.provider_id(), "").await {
            if !record.is_null() {
                return Some((session_of_record(&record, region), proxy_of(&record)));
            }
        }
        // 一条账号都没有 → 环境变量旁路
        return region.env_token().map(|token| {
            let session = serde_json::json!({
                "auth": { "accessToken": token },
                "account": {},
            });
            (session, None)
        });
    }
    match store::record(ctx, region.provider_id(), requested).await {
        Ok(record) if !record.is_null() => {
            Some((session_of_record(&record, region), proxy_of(&record)))
        }
        _ => {
            logging::verbose(
                "[Models]",
                &format!("模型目录刷新失败：指定的账号不存在或不可用（{requested}）"),
            );
            None
        }
    }
}

/// 记录上的代理（`WireProxy` 形状与网关 `ResolvedProxy` 同构）。
fn proxy_of(record: &Value) -> Option<WireProxy> {
    record
        .get("proxy")
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

/// 会话形态的折叠（与 `mod.rs` 的 `session_of_record` 同一份规则）。
///
/// 这里重写一份而不是调 `super::session_of_record`：那个函数是 `mod.rs` 私有的
/// 小工具，`catalog` 与 `keepalive` 都要用，放在这里会让 `mod.rs` 反向依赖
/// 子模块 —— 而这个折叠只有五行，抄一份的成本低于绕一圈的耦合。
fn session_of_record(record: &Value, region: Region) -> Value {
    let mut session = serde_json::Map::new();
    let mut account = serde_json::Map::new();
    for key in ["uid", "enterpriseId", "type", "departmentFullName", "name"] {
        if let Some(value) = record.get(key).filter(|value| !value.is_null()) {
            account.insert(key.to_string(), value.clone());
        }
    }
    let mut auth_map = serde_json::Map::new();
    for key in ["accessToken", "refreshToken", "expiresAt", "refreshExpiresAt", "domain"] {
        if let Some(value) = record.get(key).filter(|value| !value.is_null()) {
            auth_map.insert(key.to_string(), value.clone());
        }
    }
    session.insert("account".to_string(), Value::Object(account));
    session.insert("auth".to_string(), Value::Object(auth_map));
    if let Some(endpoint) = record.get("endpoint").filter(|value| !value.is_null()) {
        session.insert("endpoint".to_string(), endpoint.clone());
    }
    session.insert(
        "edition".to_string(),
        Value::String(region.edition_id().to_string()),
    );
    Value::Object(session)
}

/// 拿一份会话去打上游（两步：`/v3/config`，企业账号再补一跳）。
async fn refresh_for_session(
    region: Region,
    session: &Value,
    proxy: Option<&WireProxy>,
) -> CatalogOutcome {
    let base = session_endpoint(session, region);
    let headers = auth::build_auth_headers(session);
    let has_auth = headers
        .iter()
        .any(|(key, value)| key.eq_ignore_ascii_case("authorization") && !value.trim().is_empty());
    if base.is_empty() || !has_auth {
        return CatalogOutcome::failed(REASON_NO_SESSION);
    }

    // ── 第一跳：`/v3/config` ────────────────────────────────
    // UA 必须带（服务端按 UA 识别客户端通道，缺失时只下发旧版兼容清单）。
    let mut request_headers: Vec<(String, String)> =
        vec![("User-Agent".to_string(), user_agent_for_edition(Some(region.id())))];
    request_headers.extend(headers.iter().cloned());

    let url = format!("{base}/v3/config");
    match send_raw(
        "GET",
        &url,
        None,
        &request_headers,
        proxy,
        Some(REFRESH_TIMEOUT_MS),
    )
    .await
    {
        // 不校验 HTTP 状态：与网关一致 —— 先按 JSON 解析，再看有没有可用模型
        Ok(response) => {
            if let Some(payload) = response.payload.as_ref() {
                let raw = payload
                    .get("data")
                    .and_then(|data| data.get("models"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let allowed = builtin::apply_filters(raw.clone());
                if !allowed.is_empty() {
                    let picker = picker_model_ids(payload);
                    let models = annotate(allowed, picker.as_deref());
                    logging::log(
                        "[Models]",
                        &format!(
                            "✅ {} 模型目录已更新（{} 个，来源 v3/config）",
                            region.label(),
                            models.len()
                        ),
                    );
                    return CatalogOutcome::ok(models);
                }
                logging::verbose(
                    "[Models]",
                    &format!("远程清单({} 个)不含可用对话模型，保留现有目录", raw.len()),
                );
            } else {
                logging::verbose("[Models]", "远程配置拉取失败: 响应不是合法 JSON");
            }
        }
        Err(error) => logging::verbose(
            "[Models]",
            &format!("远程配置拉取失败: {}", crate::http::describe_chain(&error)),
        ),
    }

    // ── 第二跳：企业账号的模型清单 ──────────────────────────
    let enterprise_id = session
        .get("account")
        .and_then(|account| account.get("enterpriseId"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if !enterprise_id.is_empty() {
        let url = format!(
            "{base}/console/enterprises/{}/config/models",
            encode_uri_component(&enterprise_id)
        );
        return match send_raw(
            "GET",
            &url,
            None,
            &headers,
            proxy,
            Some(REFRESH_TIMEOUT_MS),
        )
        .await
        {
            Ok(response) => {
                let raw = response
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("data"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let allowed = builtin::apply_filters(raw);
                if allowed.is_empty() {
                    CatalogOutcome::failed("企业模型列表为空")
                } else {
                    // 企业分支没有 picker 名单 → 不标可见性（见 `annotate`）
                    let models = annotate(allowed, None);
                    logging::log(
                        "[Models]",
                        &format!(
                            "✅ {} 模型目录已更新（{} 个，来源 enterprise）",
                            region.label(),
                            models.len()
                        ),
                    );
                    CatalogOutcome::ok(models)
                }
            }
            Err(error) => CatalogOutcome::failed(crate::http::describe_chain(&error)),
        };
    }

    CatalogOutcome::failed(REASON_NO_SESSION)
}

/// 会话生效端点（账号记录的 `endpoint` 覆盖优先，兜底本地区端点）。
fn session_endpoint(session: &Value, region: Region) -> String {
    let fallback = region
        .env_endpoint_override()
        .unwrap_or_else(|| region.default_endpoint().to_string());
    session
        .get("endpoint")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(normalize_endpoint)
        .unwrap_or_else(|| normalize_endpoint(&fallback))
}

/// 远程清单的落库形态（网关 `apply_remote_with_picker` 的逐字副本）。
///
/// 三件事：
///   1. `isDefault` 统一为「是不是 `auto`」（远程可能把 fast-model 标成默认，
///      与网关默认不一致）；
///   2. `modelType` 写 `auto` / `built-in`（企业分支已在上游区分，这里不重复）；
///   3. `visibility`：在不在官方 picker 名单里。`picker` 为 `None` 时**不写**
///      这个键 —— 「没依据就不下判断」（与 Trae 的「缺省 = 可见」同一纪律）。
fn annotate(models: Vec<Value>, picker: Option<&[String]>) -> Vec<Value> {
    models
        .into_iter()
        .map(|model| {
            let raw_id = model.get("id").map(builtin::value_text).unwrap_or_default();
            let is_auto = raw_id.to_lowercase() == "auto";
            let mut object = match model {
                Value::Object(map) => map,
                other => return other,
            };
            object.insert("isDefault".to_string(), Value::Bool(is_auto));
            object.insert(
                "modelType".to_string(),
                Value::String(if is_auto { "auto" } else { "built-in" }.to_string()),
            );
            if let Some(list) = picker {
                let visible = list.iter().any(|id| id.eq_ignore_ascii_case(&raw_id));
                object.insert(
                    "visibility".to_string(),
                    Value::String(if visible { "shown" } else { "hidden" }.to_string()),
                );
            }
            Value::Object(object)
        })
        .collect()
}

/// 从 `/v3/config` 响应里取**官方 picker 名单**（`agents[name=="cli"].models`）。
///
/// ── 形态（2026-10-17 实测）──────────────────────────────────
/// `data.agents` 是对象数组，其中 `name == "cli"` 那组的 `models` 是**裸字符串
/// 数组**（18 条），而 `data.models` 是 60 条带元数据的对象数组 —— 其余 42 条
/// 是补全 / 改写 / 内部 / 图像 / 视频 / 旧代条目，官方 picker 不列。
///
/// ── 兼容形态 ────────────────────────────────────────────────
/// `agents` 是**对象**（`{name: {...}}`）时同样接受（桌面端缓存文件是那个形状）。
fn picker_model_ids(payload: &Value) -> Option<Vec<String>> {
    let agents = payload.get("data")?.get("agents")?;
    let collect = |name: &str, entry: &Value| -> Option<Vec<String>> {
        if !name.trim().eq_ignore_ascii_case("cli") {
            return None;
        }
        let models = entry.get("models")?.as_array()?;
        let ids: Vec<String> = models
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect();
        if ids.is_empty() { None } else { Some(ids) }
    };
    if let Some(rows) = agents.as_array() {
        for row in rows {
            let name = row.get("name").and_then(Value::as_str).unwrap_or("");
            if let Some(ids) = collect(name, row) {
                return Some(ids);
            }
        }
        return None;
    }
    if let Some(map) = agents.as_object() {
        for (name, row) in map {
            if let Some(ids) = collect(name, row) {
                return Some(ids);
            }
        }
    }
    None
}

/// 对照 JS 的 `encodeURIComponent`（企业 id 进 URL 路径前编码）。
fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric()
            || matches!(ch, '-' | '_' | '.' | '!' | '~' | '*' | '\'' | '(' | ')')
        {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 客户端版本常量在这里引用一次，避免 `endpoints` 的那个常量被误判成未使用
/// （它是登录链路的版本号，插件侧只读不写）。
#[allow(dead_code)]
const _CLIENT_VERSION: &str = WORKBUDDY_CLIENT_VERSION;
