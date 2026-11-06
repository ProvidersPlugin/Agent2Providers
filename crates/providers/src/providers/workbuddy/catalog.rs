//! 公共配置与企业目录分别读取、合并；企业结果只授权当前账号。

use std::collections::HashSet;

use agent2api_provider_protocol::WireProxy;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::{builtin, catalog_access, catalog_state, region::Region};
use crate::{http::send_raw, logging, store};

const TIMEOUT_MS: u64 = 20_000;
const ENTERPRISE_TIMEOUT_MS: u64 = 5_000;
const NO_SESSION: &str = "缺少登录态，使用内置模型目录";

pub struct CatalogOutcome {
    pub refreshed: bool,
    pub models: Vec<Value>,
    pub reason: String,
}

impl CatalogOutcome {
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

pub async fn refresh(ctx: &PluginCtx, region: Region, account: &str) -> CatalogOutcome {
    let requested = account.trim();
    if !requested.is_empty() {
        let Ok(record) = store::record(ctx, region.provider_id(), requested).await else {
            return CatalogOutcome::failed("指定账号不存在或不可用");
        };
        let session = super::session_of_record(&record, region);
        let proxy = match super::contract::proxy_of(&record) {
            Ok(proxy) => proxy,
            Err(error) => return CatalogOutcome::failed(error.message),
        };
        return refresh_session(ctx, region, &session, proxy.as_ref()).await;
    }
    let records = match ctx.store_list(region.provider_id()).await {
        Ok(records) => records,
        Err(_) => return CatalogOutcome::failed("读取模型目录账号列表失败"),
    };
    catalog_state::current(ctx, region).await;
    let mut refreshed = false;
    let mut reason = NO_SESSION.to_string();
    for entry in records
        .iter()
        .filter(|entry| entry.public.get("enabled").and_then(Value::as_bool) != Some(false))
    {
        let record = store::record_of(entry);
        let session = super::session_of_record(&record, region);
        let proxy = match super::contract::proxy_of(&record) {
            Ok(proxy) => proxy,
            Err(error) => return CatalogOutcome::failed(error.message),
        };
        let route = match agent2api_provider_sdk::egress::from_account(entry) {
            Ok(route) => route,
            Err(_) => {
                reason = "账号代理不可用，请检查账号设置".to_string();
                continue;
            }
        };
        let result = agent2api_provider_sdk::egress::scope(
            Some(route), refresh_session(ctx, region, &session, proxy.as_ref()),
        ).await;
        refreshed |= result.refreshed;
        if !result.reason.is_empty() {
            reason = result.reason;
        }
    }
    if records.is_empty() {
        if let Some(token) = region.env_token() {
            let session =
                json!({ "auth": { "accessToken": token }, "account": {}, "edition": region.id() });
            let result = refresh_session(ctx, region, &session, None).await;
            refreshed |= result.refreshed;
            if !result.reason.is_empty() {
                reason = result.reason;
            }
        }
    }
    if refreshed {
        CatalogOutcome::ok(catalog_state::current(ctx, region).await.models)
    } else {
        CatalogOutcome::failed(reason)
    }
}

pub(super) async fn refresh_session(
    ctx: &PluginCtx,
    region: Region,
    session: &Value,
    proxy: Option<&WireProxy>,
) -> CatalogOutcome {
    catalog_state::current(ctx, region).await;
    let base = super::session_endpoint_of(session, region);
    let headers = super::contract::framework_headers(session, region, true);
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("authorization"))
    {
        return CatalogOutcome::failed(NO_SESSION);
    }
    let v3_url = format!("{base}/v3/config");
    let primary = fetch(&v3_url, &headers, proxy, TIMEOUT_MS, false);
    let enterprise = catalog_access::enterprise(session);
    let (primary, secondary) = if enterprise {
        let id = session
            .pointer("/account/enterpriseId")
            .and_then(Value::as_str)
            .unwrap_or("");
        let encoded: String = url::form_urlencoded::byte_serialize(id.as_bytes()).collect();
        let url = format!("{base}/console/enterprises/{encoded}/config/models");
        let (primary, secondary) = tokio::join!(
            primary,
            fetch(&url, &headers, proxy, ENTERPRISE_TIMEOUT_MS, true)
        );
        (primary, Some(secondary))
    } else {
        (primary.await, None)
    };
    let complete = primary.is_ok() && secondary.as_ref().is_none_or(Result::is_ok);
    let mut models = Vec::new();
    let mut seen = HashSet::new();
    let mut failures = Vec::new();
    for result in std::iter::once(primary).chain(secondary) {
        match result {
            Ok(rows) => {
                for row in rows {
                    let id = row
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if !id.is_empty() && seen.insert(id) {
                        models.push(row);
                    }
                }
            }
            Err(reason) => failures.push(reason),
        }
    }
    if models.is_empty() && !complete {
        return CatalogOutcome::failed(if failures.is_empty() {
            "模型目录没有可用对话模型".into()
        } else {
            failures.join("；")
        });
    }
    if let Err(error) = catalog_access::save(ctx, session, &models, complete).await {
        return CatalogOutcome::failed(error.message);
    }
    if enterprise {
        catalog_state::merge_account(region, &models);
    } else {
        catalog_state::store_public(region, models.clone());
    }
    if !failures.is_empty() {
        logging::verbose(
            "[Models]",
            &format!("目录部分更新：{}", failures.join("；")),
        );
    }
    logging::log(
        "[Models]",
        &format!(
            "✅ {} 模型目录已更新（{} 个）",
            region.label(),
            models.len()
        ),
    );
    CatalogOutcome::ok(models)
}

async fn fetch(
    url: &str,
    headers: &[(String, String)],
    proxy: Option<&WireProxy>,
    timeout: u64,
    enterprise: bool,
) -> Result<Vec<Value>, String> {
    let response = send_raw("GET", url, None, headers, proxy, Some(timeout))
        .await
        .map_err(|error| crate::http::describe_chain(&error))?;
    let payload = response
        .payload
        .ok_or_else(|| "模型目录不是合法 JSON".to_string())?;
    if !response.ok
        || payload.get("code").is_some_and(|code| {
            code.as_i64()
                .or_else(|| code.as_str().and_then(|code| code.parse().ok()))
                .is_some_and(|code| code != 0)
        })
    {
        return Err(format!("模型目录请求失败（HTTP {}）", response.status));
    }
    let raw = if enterprise {
        payload.get("data")
    } else {
        payload.pointer("/data/models")
    }
    .and_then(Value::as_array)
    .ok_or_else(|| "模型目录缺少模型数组".to_string())?;
    let picker = if enterprise {
        None
    } else {
        picker_model_ids(&payload)
    };
    let allowed: Vec<Value> = raw
        .iter()
        .filter(|model| model.get("disabled").and_then(Value::as_bool) != Some(true))
        .filter(|model| {
            if !enterprise {
                return true;
            }
            // 企业目录只授权带 `chat` 标签的条目；图像条目带的是
            // `text-to-image` / `image-to-image`，漏掉它们会让企业账号
            // 看不到自己的图片模型（`is_chat_model` 也认不出图像条目）。
            model
                .get("tags")
                .and_then(Value::as_array)
                .is_some_and(|tags| {
                    tags.iter().any(|tag| tag.as_str() == Some("chat"))
                        || super::image_generation::is_image(model)
                })
        })
        .cloned()
        .collect();
    let mut models: Vec<Value> = builtin::apply_filters(
        // 带图像标签的条目**只**走图片通道：上游若哪天给某个 id 同时打上 `chat`
        // 与 `text-to-image`（或换掉 `hunyuan-image` 这个排除前缀），两边都收就会
        // 得到同一个 id 的两条不同形态，而网关按 id 取**第一条** —— 拿到对话形态
        // 的那条时 `supports_image_generation` 是 false，生图静默失效。
        allowed
            .iter()
            .filter(|model| !super::image_generation::is_image(model))
            .cloned()
            .collect(),
    )
    .into_iter()
    .map(|mut model| {
            let id = model
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let auto = id.eq_ignore_ascii_case("auto");
            model["isDefault"] = json!(auto);
            model["modelType"] = json!(if auto {
                "auto"
            } else if enterprise {
                "custom"
            } else {
                "built-in"
            });
            if let Some(picker) = &picker {
                model["visibility"] = json!(if picker
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&id))
                {
                    "shown"
                } else {
                    "hidden"
                });
            }
            model
        })
        .collect();
    // 图像模型单独一支：它们不进对话清单（`is_chat_model` 只服务对话），
    // 但必须出现在 `/v1/models` 里，否则网关的 `operation_providers` 找不到
    // 「哪条模型能走哪条图片通路」。可见性/默认项那几格不套在它们身上
    // （官方客户端的对话模型选择器本来就不列它们，标 hidden 会读成「不可用」）。
    models.extend(super::image_generation::pick(&allowed));
    Ok(models)
}

fn picker_model_ids(payload: &Value) -> Option<Vec<String>> {
    let agents = payload.pointer("/data/agents")?;
    let cli = if let Some(rows) = agents.as_array() {
        rows.iter()
            .find(|row| row.get("name").and_then(Value::as_str) == Some("cli"))?
    } else {
        agents.get("cli")?
    };
    Some(
        cli.get("models")?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect(),
    )
}
