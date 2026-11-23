//! Kiro 模型目录：`List-Available-Models`（API key 走控制面的
//! `ListAvailableModels` 分页）。
//!
//! ── 上游形状 ────────────────────────────────────────────────
//! ```json
//! {"models": [{"modelId":"claude-sonnet-4.5","modelName":"Claude Sonnet 4.5",
//!   "tokenLimits":{"maxInputTokens":200000,"maxOutputTokens":64000},
//!   "supportedInputTypes":["TEXT","IMAGE"],
//!   "additionalModelRequestFieldsSchema":{…}}],
//!  "defaultModel":{"modelId":"auto"}}
//! ```
//!
//! ── 模型记录（网关认的键）────────────────────────────────────
//! `id` / `name` / `maxInputTokens` / `maxOutputTokens` / `supportsToolCall` /
//! `supportsReasoning` / `supportsImages` / `isDefault`。
//!
//! ── 缓存 ────────────────────────────────────────────────────
//! 进程内按账号保存（不同订阅能看到的模型不同），10 分钟 TTL；手动刷新绕过。
//! 不落盘 —— 网关自己会缓存插件返回的清单。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use agent2api_provider_sdk::PluginCtx;
use crate::http::AccountRequest;
use serde_json::{json, Value};

use super::credentials::Credentials;
use super::{endpoints, refresh};
use crate::logging;
use agent2api_provider_protocol as proto;

/// 自动路径的缓存 TTL。
const TTL_MS: i64 = 10 * 60 * 1000;

/// 自带思考档位的模型（与内置实现同口径：Claude 与 Auto）。
pub fn thinks(model_id: &str) -> bool {
    model_id.to_ascii_lowercase().contains("claude") || model_id == "auto"
}

/// Kiro 侧把思考档位当字段发（自适应思考）的模型信息。
#[derive(Clone, Debug)]
pub struct NativeEffort {
    pub path: String,
    pub levels: Vec<String>,
}

#[derive(Clone)]
struct Catalog {
    models: Vec<Value>,
    fetched_at: i64,
    natives: HashMap<String, NativeEffort>,
    windows: HashMap<String, i64>,
}

fn cache() -> &'static Mutex<HashMap<String, Catalog>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Catalog>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_cache<T>(action: impl FnOnce(&mut HashMap<String, Catalog>) -> T) -> T {
    match cache().lock() {
        Ok(mut table) => action(&mut table),
        Err(poisoned) => action(&mut poisoned.into_inner()),
    }
}

fn now_ms() -> i64 {
    super::credentials::now_ms()
}

/// 内置兜底：签到登录前 `auto` 可用（Kiro 自己的路由模型）。
pub fn fallback_models() -> Vec<Value> {
    vec![json!({
        "id": "auto",
        "name": "Auto",
        "supportsToolCall": true,
        "supportsReasoning": true,
        "isDefault": true,
    })]
}

/// 读一份缓存（过期不返回）。
fn cached(account: &str) -> Option<Catalog> {
    with_cache(|table| {
        let entry = table.get(account)?;
        (now_ms() - entry.fetched_at < TTL_MS).then(|| entry.clone())
    })
}

fn store(account: &str, catalog: Catalog) {
    with_cache(|table| {
        table.insert(account.to_string(), catalog);
    });
}

/// 一个模型的可读信息（思考档位要它）。
pub fn window_of(account: &str, model: &str) -> i64 {
    with_cache(|table| {
        table
            .get(account)
            .and_then(|catalog| catalog.windows.get(model).copied())
            .unwrap_or(0)
    })
}

/// 某个模型的原生思考档位（没有 = 走 budget 前缀）。
pub fn native_of(account: &str, model: &str) -> Option<NativeEffort> {
    with_cache(|table| {
        table
            .get(account)
            .and_then(|catalog| catalog.natives.get(model).cloned())
    })
}

/// 当前清单（缓存优先；没有缓存时用兜底）。
pub fn list(account: &str) -> proto::ModelList {
    match cached(account) {
        Some(catalog) => proto::ModelList {
            models: catalog.models,
            meta: proto::CatalogMeta {
                remote: true,
                refreshed_at: catalog.fetched_at,
            },
        },
        None => proto::ModelList {
            models: fallback_models(),
            meta: Default::default(),
        },
    }
}

async fn fetch_with(
    account_id: &str,
    credentials: &Credentials,
    gateway_proxy: &str,
) -> Result<
    (
        Vec<Value>,
        HashMap<String, NativeEffort>,
        HashMap<String, i64>,
    ),
    String,
> {
    let region = credentials.region();
    let payload = if credentials.is_api_key() {
        key_models(credentials, &region, gateway_proxy).await?
    } else {
        management_models(credentials, &region, gateway_proxy).await?
    };
    let rows = payload
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let default_id = payload
        .pointer("/defaultModel/modelId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut models = Vec::new();
    let mut natives = HashMap::new();
    let mut windows = HashMap::new();
    for row in &rows {
        let Some(model_id) = row.get("modelId").and_then(Value::as_str) else {
            continue;
        };
        if model_id.is_empty() {
            continue;
        }
        let name = row
            .get("modelName")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && *value != model_id)
            .unwrap_or(if model_id == "auto" { "Auto" } else { model_id });
        let context = row
            .pointer("/tokenLimits/maxInputTokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let output = row
            .pointer("/tokenLimits/maxOutputTokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let images = row
            .get("supportedInputTypes")
            .and_then(Value::as_array)
            .is_some_and(|types| {
                types.iter().any(|value| {
                    value
                        .as_str()
                        .is_some_and(|text| text.eq_ignore_ascii_case("IMAGE"))
                })
            });
        let mut record = json!({
            "id": model_id,
            "name": name,
            "maxInputTokens": context,
            "maxOutputTokens": output,
            "supportsToolCall": true,
            "supportsReasoning": thinks(model_id),
            "isDefault": !default_id.is_empty() && model_id == default_id,
        });
        if images {
            record["supportsImages"] = json!(true);
        }
        if model_id == "auto" || !default_id.is_empty() {
            record["isDefault"] =
                json!(model_id == default_id || (default_id.is_empty() && model_id == "auto"));
        }
        if context > 0 {
            windows.insert(model_id.to_string(), context);
        }
        if let Some(native) = native_of_schema(row.get("additionalModelRequestFieldsSchema")) {
            natives.insert(model_id.to_string(), native);
        }
        if model_id == default_id {
            models.insert(0, record);
        } else {
            models.push(record);
        }
    }
    if models.is_empty() {
        return Err("Kiro 没有返回任何模型".to_string());
    }
    let _ = account_id;
    Ok((models, natives, windows))
}

/// 拉取并写入缓存。
pub async fn refresh(
    ctx: &PluginCtx,
    account: &str,
    force: bool,
) -> Result<(proto::RefreshOutcome, Vec<Value>), String> {
    let (account_id, credentials) = refresh::ensure(ctx, account).await?;
    if !force {
        if let Some(catalog) = cached(&account_id) {
            return Ok((proto::RefreshOutcome::unchanged(), catalog.models));
        }
    }
    let (models, natives, windows) = fetch_with(&account_id, &credentials, &ctx.env.proxy).await?;
    let count = models.len();
    store(
        &account_id,
        Catalog {
            models: models.clone(),
            fetched_at: now_ms(),
            natives,
            windows,
        },
    );
    logging::verbose("kiro", &format!("模型目录已刷新：{count} 个"));
    Ok((proto::RefreshOutcome::refreshed(count), models))
}

/// 管理面（OAuth 登录态）：`GET /List-Available-Models`。
async fn management_models(
    credentials: &Credentials,
    region: &str,
    gateway_proxy: &str,
) -> Result<Value, String> {
    let client = crate::http::control_client(None, gateway_proxy);
    let mut url = format!(
        "{}/List-Available-Models?origin=KIRO_CLI",
        endpoints::management(region)
    );
    if !credentials.profile.is_empty() {
        url.push_str("&profileArn=");
        url.push_str(&urlencode(&credentials.profile));
    }
    let mut request = client
        .get(&url)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {}", credentials.access))
        .timeout(std::time::Duration::from_millis(20_000));
    if let Some(kind) = credentials.token_type() {
        request = request.header("tokentype", kind);
    }
    let response = request
        .with_account_user_agent().send()
        .await
        .map_err(|error| format!("拉取 Kiro 模型目录失败：{}", crate::http::describe(&error)))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(format!("拉取 Kiro 模型目录失败（HTTP {status}）"));
    }
    serde_json::from_str(&text).map_err(|_| "Kiro 模型目录不是有效 JSON".to_string())
}

/// API key：控制面的 `ListAvailableModels`（按 nextToken 分页）。
async fn key_models(
    credentials: &Credentials,
    region: &str,
    gateway_proxy: &str,
) -> Result<Value, String> {
    let client = crate::http::control_client(None, gateway_proxy);
    let mut models: Vec<Value> = Vec::new();
    let mut default_model = Value::Null;
    let mut next: Option<String> = None;
    for _ in 0..20 {
        let mut body = json!({ "origin": "KIRO_CLI" });
        if let Some(token) = &next {
            body["nextToken"] = json!(token);
        }
        let url = format!("{}/?origin=KIRO_CLI", endpoints::management(region));
        let response = client
            .post(&url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-amz-json-1.0")
            .header("X-Amz-Target", endpoints::TARGET_KEY_MODELS)
            .header("x-amzn-codewhisperer-optout", "false")
            .header("Authorization", format!("Bearer {}", credentials.access))
            .header("tokentype", "API_KEY")
            .timeout(std::time::Duration::from_millis(20_000))
            .json(&body)
            .with_account_user_agent().send()
            .await
            .map_err(|error| {
                format!("拉取 Kiro 模型目录失败：{}", crate::http::describe(&error))
            })?;
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(format!("拉取 Kiro 模型目录失败（HTTP {status}）"));
        }
        let page: Value =
            serde_json::from_str(&text).map_err(|_| "Kiro 模型目录不是有效 JSON".to_string())?;
        if let Some(list) = page.get("models").and_then(Value::as_array) {
            models.extend(list.iter().cloned());
        }
        if default_model.is_null() {
            if let Some(value) = page.get("defaultModel") {
                if !value.is_null() {
                    default_model = value.clone();
                }
            }
        }
        next = page
            .get("nextToken")
            .and_then(Value::as_str)
            .map(str::to_string);
        if next.is_none() {
            break;
        }
    }
    Ok(json!({ "models": models, "defaultModel": default_model }))
}

/// 从目录的 schema 里读出自适应思考的档位（与 kiro-cli 同口径）。
fn native_of_schema(schema: Option<&Value>) -> Option<NativeEffort> {
    let schema = schema?;
    if !schema.is_object() {
        return None;
    }
    let types = schema
        .pointer("/properties/thinking/properties/type/enum")
        .and_then(Value::as_array)?;
    let adaptive = types.iter().any(|value| value.as_str() == Some("adaptive"));
    if !adaptive {
        return None;
    }
    for path in ["output_config", "reasoning"] {
        if let Some(levels) = schema
            .pointer(&format!("/properties/{path}/properties/effort/enum"))
            .and_then(Value::as_array)
        {
            let levels: Vec<String> = levels
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            if !levels.is_empty() {
                return Some(NativeEffort {
                    path: path.to_string(),
                    levels,
                });
            }
        }
    }
    None
}

/// 简单的 query 编码（只用于 profile ARN 这类字符集）。
fn urlencode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}
