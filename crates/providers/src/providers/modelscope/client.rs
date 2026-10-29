//! 网页站使用会话 Cookie，推理目录使用 Bearer；两种响应信封的字段大小写不同。
//! 401 表示凭证失效，403 也可能是权限或风控拒绝，不能一律提示重新登录。

use agent2api_provider_protocol::WireProxy;
use serde_json::Value;

use super::region::Region;
use crate::http;
use crate::proxy_util::{resolve_account_proxy, ProxyResolution};

pub struct ControlClient {
    region: Region,
    token: String,
    http: reqwest::Client,
}

impl ControlClient {
    pub fn new(
        region: Region,
        token: String,
        proxy: Option<&WireProxy>,
        gateway_proxy: &str,
    ) -> Result<Self, String> {
        if token.trim().is_empty() {
            return Err("ModelScope 账号缺少令牌，无法查询".to_string());
        }
        if let Some(proxy) = proxy {
            reqwest::Proxy::all(proxy.to_url())
                .map_err(|_| "ModelScope 账号代理地址无效".to_string())?;
        }
        Ok(Self { region, token, http: http::control_client(proxy, gateway_proxy) })
    }

    pub fn from_store(
        ctx: &agent2api_provider_sdk::PluginCtx,
        region: Region,
        account: &agent2api_provider_sdk::StoreAccount,
    ) -> Result<Self, String> {
        let record = crate::store::record_of(account);
        let proxy = match resolve_account_proxy(record.get("proxy")) {
            Some(ProxyResolution::Resolved(proxy)) => Some(proxy),
            Some(ProxyResolution::Failed(error)) => return Err(error),
            None => None,
        };
        Self::new(region, super::credentials::token_of(&record)?, proxy.as_ref(), &ctx.env.proxy)
    }

    pub async fn for_account(
        ctx: &agent2api_provider_sdk::PluginCtx,
        region: Region,
        account: &str,
    ) -> Result<Self, String> {
        let found = ctx.store_get(region.provider_id(), account).await.map_err(|error| error.to_string())?;
        Self::from_store(ctx, region, &found)
    }

    pub async fn site_json(&self, path: &str, operation: &str) -> Result<Value, String> {
        let headers = vec![("Cookie".to_string(), format!("m_session_id={}", self.token))];
        let (status, payload) = http::get_json(&self.http, &site_url(self.region, path), &headers)
            .await.map_err(|error| format!("{operation}失败：{error}"))?;
        self.checked(status, payload, operation, true)
    }

    pub async fn models_json(&self, path: &str) -> Result<Value, String> {
        let headers = vec![("Authorization".to_string(), format!("Bearer {}", self.token))];
        let url = format!("{}{path}", self.region.inference_base_url());
        let (status, payload) = http::get_json(&self.http, &url, &headers).await?;
        self.checked(status, payload, "模型目录查询", false)
    }

    fn checked(&self, status: u16, payload: Value, operation: &str, site: bool) -> Result<Value, String> {
        if status == 401 { return Err(expired_message(self.region, status, site)); }
        if status == 403 {
            return Err(format!("{operation}被上游拒绝（HTTP 403），请检查账号权限或稍后重试"));
        }
        if !(200..300).contains(&status) { return Err(format!("{operation}返回 HTTP {status}")); }
        if !payload.is_object() && !(!site && payload.is_array()) {
            return Err(format!("{operation}返回无法识别的响应"));
        }
        if !succeeded(&payload) {
            return Err(format!("{operation}失败（Code={}）：{}", code_of(&payload), message_of(&payload)));
        }
        if site && !data_of(&payload).is_object() {
            return Err(format!("{operation}响应缺少业务数据"));
        }
        Ok(payload)
    }
}


/// 响应信封的成功位；推理目录不带业务信封，交给调用方校验 data。
pub fn succeeded(payload: &Value) -> bool {
    for key in ["Success", "success"] {
        if let Some(flag) = payload.get(key) {
            return flag.as_bool() == Some(true);
        }
    }
    match payload.get("Code").or_else(|| payload.get("code")) {
        Some(code) => matches!(code.as_i64(), Some(0 | 200)),
        None => true,
    }
}

/// 响应信封里的**业务数据**（`Data` / `data` 两层都认）。
pub fn data_of(payload: &Value) -> Value {
    payload
        .get("Data")
        .or_else(|| payload.get("data"))
        .cloned()
        .unwrap_or(Value::Null)
}

/// 响应信封里的**上游文案**（`Message` / `message` / `msg` 三种都认）。
pub fn message_of(payload: &Value) -> String {
    payload
        .get("Message")
        .or_else(|| payload.get("message"))
        .or_else(|| payload.get("msg"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// 响应信封里的业务码（`Code` / `code`；都没有时给 0）。
pub fn code_of(payload: &Value) -> i64 {
    payload
        .get("Code")
        .or_else(|| payload.get("code"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

/// 拼一个网页站 URL（`region` 决定站点）
pub fn site_url(region: Region, path: &str) -> String {
    format!("{}{path}", region.site_base_url())
}

/// HTTP 401 的令牌失效文案；错误站点也可能返回 401，因此同时提示站点不匹配。
pub fn expired_message(region: Region, status: u16, site: bool) -> String {
    if site {
        format!(
            "ModelScope 令牌已失效或不属于该站点（{}），请重新粘贴（HTTP {status}）",
            region.site_base_url()
        )
    } else {
        format!("ModelScope 令牌已失效，请重新粘贴（HTTP {status}）")
    }
}
