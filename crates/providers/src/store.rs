//! 账号凭证的读写：插件侧的统一入口。
//!
//! ── 为什么要有这一层 ────────────────────────────────────────
//! 从网关搬来的各家 `credentials.rs` 都长成同一个形状：`from_record(Some(&record))`
//! 里用 `record.get("accessToken")` 这样的取值链。搬家时如果每家都改一遍取值
//! 方式，diff 会淹没真正的逻辑改动 —— 而逐字核对正是迁移验收的手段。
//!
//! 所以这里做一件事：把「插件的不透明 secret」**拼成网关记录的形状**（把
//! 账号 id 并进顶层），于是 `from_record` 可以逐字保留，只是数据来源从
//! `store.xxx_account_record(id)` 换成 [`record`]。
//!
//! ── 字段名为什么沿用网关的驼峰名 ────────────────────────────
//! `secret` 是插件自己定义的 JSON，用哪套键名都行。沿用 `accessToken` /
//! `refreshToken` / `expiresAt` 这些名字的好处是：搬来的代码不用改，人肉核对
//! 两份实现时字段一一对应。代价是与内置家「字段同名、语义不同」（内置家的
//! `accessToken` 在账号记录顶层，插件的在 `pluginSecret` 里）—— 但两者物理
//! 隔离（网关从不解析 `pluginSecret`），不会串。
//!
//! ── 写回为什么必须「比较再写」───────────────────────────────
//! 见 `plugin_accounts::plugin_update_secret` 的说明：两个并发刷新会互相覆盖。
//! [`save`] 强制调用方传入**读到时的那份 secret**，网关只在值没变时才写。

use std::borrow::Cow;

use agent2api_provider_sdk::{PluginCtx, StoreAccount, StoreProxy};
use serde_json::{Map, Value};

use crate::errors::GatewayError;
use crate::proxy_util::{resolve_account_proxy, ProxyResolution};

/// 读一条账号的 secret，并拼成「网关记录形态」（顶层带 `id`）。
///
/// `account` 为空串 = 该 provider 的队首账号（与内置家的语义一致）。
pub async fn record(
    ctx: &PluginCtx,
    provider: &str,
    account: &str,
) -> Result<Value, GatewayError> {
    let found = ctx.store_get(provider, account).await.map_err(|error| {
        GatewayError::with_status(401, format!("读取账号失败：{error}"))
    })?;
    validate_secret(&found.secret)?;
    Ok(record_of(&found))
}

/// 列出一个 provider 的全部账号（记录形态，顺序即网关给的顺序）。
///
/// 网关 `list_accounts()` 的同义物：搬来的家有几处要「扫一遍账号找代表」
/// （ZCode 的配置同步、签到选路），那些判据本身是纯函数，喂一份快照就能跑。
pub async fn list(
    ctx: &PluginCtx,
    provider: &str,
) -> Result<Vec<Value>, GatewayError> {
    let accounts = ctx
        .store_list(provider)
        .await
        .map_err(|error| GatewayError::with_status(401, format!("读取账号列表失败：{error}")))?;
    Ok(accounts.iter().map(record_of).collect())
}

/// 把一条 [`StoreAccount`] 摊成「网关记录形态」。
///
/// `secret` 里的键全部提到顶层，`name` 缺失时从 `public` 补齐。
/// `id` / `provider` 与运行时出口以网关为准；被覆盖的凭证字段保留原值供条件更新。
pub fn record_of(account: &StoreAccount) -> Value {
    let mut merged = Map::new();
    let public = object_view(&account.public);
    let secret = object_view(&account.secret);
    if let Some(object) = public.as_ref() {
        if let Some(name) = object.get("name") {
            merged.insert("name".to_string(), name.clone());
        }
    }
    if let Some(object) = secret.as_ref() {
        for (key, value) in object.iter() {
            merged.insert(key.clone(), value.clone());
        }
    }
    let mut originals = Map::new();
    for (key, value) in [("id", &account.id), ("provider", &account.provider)] {
        if let Some(original) = secret.as_ref().and_then(|fields| fields.get(key)) {
            originals.insert(key.to_string(), original.clone());
        }
        merged.insert(key.to_string(), Value::String(value.clone()));
    }
    if !originals.is_empty() {
        merged.insert("_agent2apiOriginalIdentity".to_string(), Value::Object(originals));
    }
    let mut injected: Vec<&str> = ["id", "provider", "name"]
        .into_iter()
        .filter(|key| secret.as_ref().and_then(|fields| fields.get(*key)).is_none())
        .collect();
    if let Some(proxy) = &account.proxy {
        if let Some(original) = secret.as_ref().and_then(|fields| fields.get("proxy")) {
            merged.insert("_agent2apiOriginalProxy".to_string(), original.clone());
        } else {
            injected.push("proxy");
        }
        let value = match proxy {
            StoreProxy::Unset => Value::Null,
            StoreProxy::Resolved { proxy } => serde_json::json!(proxy),
            StoreProxy::Static { config } => match resolve_account_proxy(Some(config)) {
                Some(ProxyResolution::Resolved(proxy)) => serde_json::json!(proxy),
                Some(ProxyResolution::Failed(message)) => serde_json::json!({ "error": message }),
                None => Value::Null,
            },
            StoreProxy::Failed { message } => serde_json::json!({ "error": message }),
        };
        merged.insert("proxy".to_string(), value);
    }
    merged.insert(
        "_agent2apiRecordKeys".to_string(),
        serde_json::json!(injected),
    );
    if account.secret.is_string() {
        // 旧宿主的条件写回按原始 JSON 字符串比较，不能只带解析后的对象。
        merged.insert("_agent2apiOriginalSecret".to_string(), account.secret.clone());
    }
    Value::Object(merged)
}

fn object_view(value: &Value) -> Option<Cow<'_, Map<String, Value>>> {
    match value {
        Value::Object(fields) => Some(Cow::Borrowed(fields)),
        Value::String(text) => match serde_json::from_str::<Value>(text).ok()? {
            Value::Object(fields) => Some(Cow::Owned(fields)),
            _ => None,
        },
        _ => None,
    }
}

fn validate_secret(value: &Value) -> Result<(), GatewayError> {
    if object_view(value).is_some() {
        return Ok(());
    }
    Err(GatewayError::with_status(
        400,
        "账号凭据存储形态异常：pluginSecret 必须是对象或包含对象的 JSON 字符串；请检查导入记录，不要反复重新登录",
    ))
}

/// 只移除记录投影补入的字段；凭证本来含有的 name 等字段必须参与比较再写。
pub fn secret_view(record: &Value) -> Value {
    if let Some(original) = record.get("_agent2apiOriginalSecret") {
        return original.clone();
    }
    let mut secret = record.clone();
    if let Some(object) = secret.as_object_mut() {
        let injected = object.remove("_agent2apiRecordKeys");
        match injected.as_ref().and_then(Value::as_array) {
            Some(keys) => {
                for key in keys.iter().filter_map(Value::as_str) {
                    object.remove(key);
                }
            }
            None => {
                for key in ["id", "provider", "name"] {
                    object.remove(key);
                }
            }
        }
        if let Some(Value::Object(originals)) = object.remove("_agent2apiOriginalIdentity") {
            for (key, value) in originals {
                object.insert(key, value);
            }
        }
        if let Some(proxy) = object.remove("_agent2apiOriginalProxy") {
            object.insert("proxy".to_string(), proxy);
        }
    }
    secret
}

/// 读原始 secret（写回时的 `expect` 用它）。
#[allow(dead_code)]
pub async fn secret(
    ctx: &PluginCtx,
    provider: &str,
    account: &str,
) -> Result<(String, Value), GatewayError> {
    let found = ctx
        .store_get(provider, account)
        .await
        .map_err(|error| GatewayError::with_status(401, format!("读取账号失败：{error}")))?;
    Ok((found.id, found.secret))
}

/// 比较再写：只有当前 secret 仍等于 `expect` 时才写入。
///
/// 返回 `false` = 期望值已变（另一个刷新先落盘了）。那不是错误 —— 调用方
/// 应当重新读一遍再决定（与网关 `plugin_update_secret` 的语义一致）。
pub async fn save(
    ctx: &PluginCtx,
    account: &str,
    expect: &Value,
    next: &Value,
) -> Result<bool, GatewayError> {
    ctx.store_update(account, expect, next)
        .await
        .map_err(|error| GatewayError::with_status(502, format!("写回账号失败：{error}")))
}

/// 在 secret 上原地改几个字段（其余字段原样保留）。
///
/// ── 为什么不能整份替换 ──────────────────────────────────────
/// 各家 secret 里除了令牌还有身份载荷件（LobsterAI 的 uuid / keyfrom、
/// AtomCode 的 userId…）。续期只该动令牌那几个键 —— 整份替换会把身份件丢掉，
/// 而丢了它们账号就永远续不了期。
pub fn with_fields(secret: &Value, fields: &[(&str, Value)]) -> Value {
    let Some(object) = object_view(secret) else { return secret.clone() };
    let mut next = Value::Object(object.into_owned());
    let Some(object) = next.as_object_mut() else {
        return next;
    };
    for (key, value) in fields {
        object.insert((*key).to_string(), value.clone());
    }
    next
}

/// 从 secret 里取一个非空字符串（多候选键，宽容读取）。
pub fn pick(record: &Value, keys: &[&str]) -> String {
    for key in keys {
        let value = record
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !value.is_empty() {
            return value.to_string();
        }
    }
    String::new()
}

/// 令牌尾 4 字符（公开视图的展示字段）。
pub fn token_tail(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let start = chars.len().saturating_sub(4);
    chars[start..].iter().collect()
}
