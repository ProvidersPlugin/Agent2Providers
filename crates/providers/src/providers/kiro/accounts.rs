//! Kiro 账号：添加（API key / 本机登录态 / 文件）、公开视图、去重身份。
//!
//! ── 三条添加路径 ────────────────────────────────────────────
//! ```text
//!   manual     粘贴 ksk_… 的 API key（可选账号名）
//!   desktop    读本机 kiro-cli / Kiro IDE 的登录态，只记来源、不落 token
//!   file_text  kiro-auth-token.json（OAuth 凭证）或一整行 API key
//! ```
//!
//! ── 为什么 API key 也要尽力探活 ─────────────────────────────
//! 它的 profile / 邮箱 / 套餐只有问上游才知道。探活失败**不阻断添加**
//! （网络抖动不该让用户加不进账号），只是账号名回落到默认值；真正不可用的
//! key 会在第一次对话时得到明确的 401/403。

use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use super::credentials::{public as public_view, Credentials};
use super::{local, usage};
use crate::limits;

/// 从表单 / 文件内容里取一个非空字符串。
fn pick(payload: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = payload.get(*key).and_then(Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    String::new()
}

/// 归一化一条账号。
pub async fn normalize(
    ctx: &PluginCtx,
    input: agent2api_provider_protocol::NormalizeInput,
) -> Result<agent2api_provider_protocol::NormalizedAccount, String> {
    use agent2api_provider_protocol::{NormalizeSource, NormalizedAccount};
    let name = input.name.clone().unwrap_or_default();
    match input.source {
        NormalizeSource::Desktop => {
            let credentials = local::read_cli().or_else(local::read_ide).ok_or_else(|| {
                "没有找到 Kiro 的本机登录态：请先在 kiro-cli 或 Kiro IDE 里登录".to_string()
            })?;
            let source = credentials.source.clone();
            let mut secret = json!({
                "method": credentials.method,
                "source": source,
                "region": credentials.region,
                "loginProvider": credentials.login_provider,
                "uid": credentials.uid,
            });
            if !name.is_empty() {
                secret["name"] = json!(name);
            }
            // 本机登录态的邮箱 / 套餐是实时读出来的，best-effort 补进账号名。
            if let Some((email, plan)) = who_is(&credentials, &ctx.env.proxy).await {
                if !email.is_empty() {
                    secret["accountId"] = json!(email);
                }
                if !plan.is_empty() {
                    secret["plan"] = json!(plan);
                }
            }
            let view = public_view(&secret);
            Ok(NormalizedAccount {
                identity: super::credentials::identity(&secret),
                public: view,
                secret,
                desktop: true,
            })
        }
        NormalizeSource::Manual { payload } => {
            // 动态登录方式（methodIndex + inputs）把凭证包在 inputs 里；
            // 旧入口直接把字段放在顶层。两种都认。
            let fields = payload
                .get("inputs")
                .filter(|value| value.is_object())
                .unwrap_or(&payload);
            add_key(fields, &name).await
        }
        NormalizeSource::FileText { content, .. } => {
            let text = content.trim();
            if text.starts_with('{') {
                return add_json(text, &name).await;
            }
            add_key(&json!({ "apiKey": text }), &name).await
        }
        NormalizeSource::FileBase64 { .. } => {
            Err("Kiro 不支持导入二进制文件，请粘贴 API key 或导入本机登录态".to_string())
        }
    }
}

/// 粘贴 API key。
async fn add_key(
    payload: &Value,
    name: &str,
) -> Result<agent2api_provider_protocol::NormalizedAccount, String> {
    use agent2api_provider_protocol::NormalizedAccount;
    let key = pick(payload, &["apiKey", "accessToken", "token", "key"]);
    if key.is_empty() {
        return Err("请填写 Kiro API key（ksk_ 开头）".to_string());
    }
    if key.len() > limits::MAX_TOKEN_LENGTH {
        return Err("这份凭据异常地长，请确认只粘贴了 Kiro API key".to_string());
    }
    // API key 的对话与额度都**不需要 profile**（与 kiro-cli 一致：Key 的调用
    // 不带 profileArn）。所以这里不探活：添加账号是本地操作，瞬间完成；
    // 真正不可用的 key 会在第一次对话时得到明确的 401/403。
    let credentials = Credentials {
        method: "apikey".into(),
        access: key,
        region: "us-east-1".into(),
        ..Default::default()
    };
    let mut secret = json!({ "name": if name.is_empty() { "Kiro API" } else { name } });
    credentials.write_into(&mut secret);
    let view = public_view(&secret);
    Ok(NormalizedAccount {
        identity: super::credentials::identity(&secret),
        public: view,
        secret,
        desktop: false,
    })
}

/// 导入 `kiro-auth-token.json` 形态的 OAuth 凭证。
async fn add_json(
    text: &str,
    name: &str,
) -> Result<agent2api_provider_protocol::NormalizedAccount, String> {
    use agent2api_provider_protocol::NormalizedAccount;
    let value: Value = serde_json::from_str(text)
        .map_err(|_| "这不是有效的 JSON，请粘贴 kiro-auth-token.json 或 API key".to_string())?;
    let access = pick(&value, &["accessToken", "access", "token"]);
    if access.is_empty() {
        return Err("JSON 里没有 accessToken".to_string());
    }
    let mut credentials = Credentials {
        access,
        refresh: pick(&value, &["refreshToken", "refresh"]),
        expires: value
            .get("expiresAt")
            .and_then(Value::as_str)
            .and_then(crate::timeutil::parse_rfc3339_ms)
            .unwrap_or(0),
        region: {
            let region = pick(&value, &["region"]);
            if region.is_empty() {
                "us-east-1".to_string()
            } else {
                region
            }
        },
        profile: pick(&value, &["profileArn", "profile"]),
        client_id: pick(&value, &["clientId"]),
        client_secret: pick(&value, &["clientSecret"]),
        method: "idc".into(),
        login_provider: "enterprise".into(),
        uid: "enterprise".into(),
        ..Default::default()
    };
    if credentials.refresh.is_empty() {
        credentials.method = "social".into();
    }
    if credentials.client_id.is_empty() && credentials.method == "idc" {
        credentials.method = "social".into();
    }
    let mut secret = json!({ "name": if name.is_empty() { "Kiro" } else { name } });
    credentials.write_into(&mut secret);
    let view = public_view(&secret);
    Ok(NormalizedAccount {
        identity: super::credentials::identity(&secret),
        public: view,
        secret,
        desktop: false,
    })
}

/// 问一次 `Get-Usage-Limits` 拿邮箱与套餐（失败返回 None）。
async fn who_is(credentials: &Credentials, gateway_proxy: &str) -> Option<(String, String)> {
    let payload = usage::fetch_usage_limits(credentials, gateway_proxy)
        .await
        .ok()?;
    let email = payload
        .pointer("/userInfo/email")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let plan = payload
        .pointer("/subscriptionInfo/subscriptionTitle")
        .and_then(Value::as_str)
        .map(usage::plan_name)
        .unwrap_or_default();
    Some((email, plan))
}

/// 公开视图。
pub fn public(secret: &Value) -> Result<Value, String> {
    Ok(public_view(secret))
}

/// 去重身份。
pub fn identity(secret: &Value) -> String {
    super::credentials::identity(secret)
}
