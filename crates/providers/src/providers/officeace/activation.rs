use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use crate::errors::GatewayError;
use super::{control, credentials, oauth, subscription};
use credentials::OfficeAceCredential;

/// pending 标记的有效期（毫秒）：上游受理开通后迟迟不返回 `SUBSCRIBED` 时，
/// 超过此时长即视为本次开通未生效，回落到真实订阅状态并允许重新提交
const PENDING_TTL_MS: i64 = 10 * 60 * 1000;

/// 记录每个账号进入 pending 的时间（毫秒时间戳），超时回落用
fn pending_accounts() -> &'static Mutex<HashMap<String, i64>> {
    static PENDING: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

fn pending(account_id: &str) -> bool {
    let mut accounts = pending_accounts().lock().unwrap_or_else(|error| error.into_inner());
    match accounts.get(account_id) {
        Some(&since) if crate::logging::now_ms() - since < PENDING_TTL_MS => true,
        _ => {
            accounts.remove(account_id);
            false
        }
    }
}

fn set_pending(account_id: &str, value: bool) {
    let mut accounts = pending_accounts().lock().unwrap_or_else(|error| error.into_inner());
    if value {
        accounts.insert(account_id.to_string(), crate::logging::now_ms());
    } else {
        accounts.remove(account_id);
    }
}

pub async fn preview(ctx: &PluginCtx, account_id: &str) -> Result<Value, String> {
    if account_id.is_empty() {
        return Err(GatewayError::with_status(400, "免费套餐检查缺少账号").into_control_message());
    }
    let record = crate::store::record(ctx, super::ID, account_id).await
        .map_err(GatewayError::into_control_message)?;
    match preview_inner(account_id, &record).await {
        Ok(value) => Ok(value),
        Err(mut error) => {
            redact(&mut error.message, &record);
            Err(error.into_control_message())
        }
    }
}

async fn preview_inner(account_id: &str, record: &Value) -> Result<Value, GatewayError> {
    let credential = credentials::from_record(Some(record))?;
    if !credential.has_control_plane() || credential.security_token.is_empty() {
        return Ok(view("credentials_missing", "缺少套餐开通凭证，请通过果办网页登录补齐授权"));
    }
    if official_gateway(&credential.base_url).is_err() {
        return Ok(view("unsupported", "账号未使用官方果办网关地址，无法在此开通免费套餐"));
    }
    if credential.expires_at > 0 && credential.expires_at <= crate::logging::now_ms() {
        return Ok(view("credentials_expired", "套餐查询凭证已过期，请先到账号页刷新凭证；此查询不会自动续期"));
    }
    // 状态查询只读，不自动续期，也不调用开通或积分领取接口。
    let snapshot = match subscription::fetch_subscription(&credential).await {
        Ok(snapshot) => snapshot,
        Err(error) if error.code.as_deref() == Some(subscription::CONTROL_TOKEN_EXPIRED) => {
            return Ok(view("credentials_expired", "套餐查询凭证已过期，请先到账号页刷新凭证；此查询不会自动续期"));
        }
        Err(error) if matches!(error.status_code, 401 | 403) => {
            return Ok(view("permission_denied", "官方拒绝套餐查询，请检查账号授权；这不一定表示 Token 已过期"));
        }
        Err(error) => return Err(error),
    };
    let mut state = subscription_state(&snapshot);
    if state.get("status").and_then(Value::as_str) == Some("active") {
        set_pending(account_id, false);
    } else if pending(account_id) {
        state = view("pending", "开通请求仍在处理或结果待确认，请稍后查询，不要重复提交");
    }
    Ok(state)
}

fn subscription_state(snapshot: &Value) -> Value {
    match snapshot.get("subscribe_status").and_then(Value::as_str) {
        Some("SUBSCRIBED") => view("active", "套餐已开通，无需重复操作"),
        Some("UNSUBSCRIBED") => view("inactive", "尚未开通套餐，可开通免费体验套餐"),
        Some("EXPIRED") => view("expired", "原套餐已到期，请在官方客户端处理；这里不会自动续费或变更套餐"),
        Some("FROZEN") => view("frozen", "原套餐已冻结，请在官方客户端处理；这里不会自动续费或变更套餐"),
        _ => view("unknown", "官方未返回明确的套餐状态，暂不能开通"),
    }
}

fn view(status: &str, message: &str) -> Value {
    json!({
        "status": status,
        "canActivate": status == "inactive",
        "message": message,
        "checkedAt": crate::logging::now_ms(),
    })
}

pub async fn ensure(ctx: &PluginCtx, account_id: &str) -> Result<Value, String> {
    if account_id.is_empty() {
        return Err(GatewayError::with_status(400, "免费套餐检查缺少账号").into_control_message());
    }
    crate::flight::run(&format!("officeace-subscription:{account_id}"), || async {
        let record = crate::store::record(ctx, super::ID, account_id).await
            .map_err(GatewayError::into_control_message)?;
        match ensure_inner(ctx, account_id, &record).await {
            Ok(report) => Ok(report),
            Err(mut error) => {
                redact(&mut error.message, &record);
                if let Ok(latest) = crate::store::record(ctx, super::ID, account_id).await {
                    redact(&mut error.message, &latest);
                }
                Err(error.into_control_message())
            }
        }
    }).await
}

async fn ensure_inner(
    ctx: &PluginCtx,
    account_id: &str,
    record: &Value,
) -> Result<Value, GatewayError> {
    let initial = credentials::from_record(Some(record))?;
    if !initial.has_control_plane() || initial.security_token.is_empty() {
        return Ok(report("skipped", "缺少套餐开通凭证，未自动开通免费套餐；请通过果办网页登录补齐授权"));
    }
    official_gateway(&initial.base_url)?;
    let mut credential = control::credential_of(ctx, account_id).await?;
    official_gateway(&credential.base_url)?;
    let before = match subscription::fetch_subscription(&credential).await {
        Err(error) if error.code.as_deref() == Some(subscription::CONTROL_TOKEN_EXPIRED) => {
            credential = control::refresh(ctx, account_id, Some(&credential)).await?;
            subscription::fetch_subscription(&credential).await?
        }
        result => result?,
    };
    match before.get("subscribe_status").and_then(Value::as_str) {
        Some("SUBSCRIBED") => {
            set_pending(account_id, false);
            return Ok(report("already_active", "套餐已开通，无需重复操作"));
        }
        Some("UNSUBSCRIBED") if pending(account_id) => {
            return Ok(report("pending", "开通请求仍在处理或结果待确认，请稍后查询，不要重复提交"));
        }
        Some("UNSUBSCRIBED") => {}
        Some("EXPIRED" | "FROZEN") => {
            return Ok(report("skipped", "原套餐已到期或冻结，未自动变更或续费；请在官方客户端检查套餐"));
        }
        _ => return Ok(report("skipped", "官方未返回明确的套餐状态，未自动开通")),
    }

    let signing = credential.signing().map_err(GatewayError::from)?;
    let (account, principal, model_info) = oauth::fetch_permission(&signing).await
        .map_err(|message| GatewayError::with_status(502, message))?;
    for (key, actual) in [("accountId", account.as_str()), ("principalId", principal.as_str())] {
        let expected = crate::store::pick(record, &[key]);
        if !expected.is_empty() && expected != actual {
            return Err(GatewayError::with_status(409, "果办登录身份与导入记录不一致，未自动开通套餐"));
        }
    }
    if model_info.is_object() {
        let (_, key, secret) = oauth::gateway_of(&model_info)
            .map_err(|message| GatewayError::with_status(502, message))?;
        if key != credential.model_app_key || secret != credential.model_app_secret {
            return Err(GatewayError::with_status(409, "果办模型凭证与登录身份不一致，未自动开通套餐"));
        }
    }

    // 复用官方首次开通接口，只发送空对象，不传付费套餐、订单或续费参数。
    set_pending(account_id, true);
    let model_info = match oauth::subscribe(oauth::CLOUD_BASE, &signing, None).await {
        Ok(model_info) => model_info,
        Err(error) if error.code.as_deref() == Some(oauth::SUBSCRIPTION_UNCERTAIN) => {
            return Ok(report("pending", "开通请求仍在处理或结果待确认，请稍后查询，不要重复提交"));
        }
        Err(error) => {
            set_pending(account_id, false);
            return Err(error);
        }
    };
    let (base_url, model_app_key, model_app_secret) = oauth::gateway_of(&model_info)
        .map_err(|message| GatewayError::with_status(502, message))?;
    official_gateway(&base_url)?;
    save_gateway(ctx, account_id, &credential, &base_url, &model_app_key, &model_app_secret).await?;

    credential.base_url = base_url;
    credential.model_app_key = model_app_key;
    credential.model_app_secret = model_app_secret;
    let after = match subscription::fetch_subscription(&credential).await {
        Ok(after) => after,
        Err(_) => return Ok(report("pending", "免费套餐开通请求已提交，但回查失败，请稍后查询确认")),
    };
    if after.get("subscribe_status").and_then(Value::as_str) != Some("SUBSCRIBED") {
        return Ok(report("pending", "免费套餐开通请求已提交，官方尚未确认生效，请稍后查询"));
    }
    set_pending(account_id, false);
    Ok(report("activated", "免费套餐已开通并确认生效"))
}

fn official_gateway(base_url: &str) -> Result<(), GatewayError> {
    let valid = url::Url::parse(base_url).is_ok_and(|url| {
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port_or_known_default() == Some(443)
            && url.query().is_none()
            && url.fragment().is_none()
            && matches!(url.path().trim_end_matches('/'), "" | "/v2")
            && url.host_str().is_some_and(|host| {
                host.ends_with(".officeace.cn-southwest-2.huaweicloud-agentarts.com")
            })
    });
    if valid { Ok(()) } else {
        Err(GatewayError::with_status(400, "账号未使用官方果办网关地址，未自动开通免费套餐"))
    }
}

async fn save_gateway(
    ctx: &PluginCtx,
    account_id: &str,
    previous: &OfficeAceCredential,
    base_url: &str,
    app_key: &str,
    app_secret: &str,
) -> Result<(), GatewayError> {
    let record = crate::store::record(ctx, super::ID, account_id).await?;
    let current = credentials::from_record(Some(&record))?;
    if current.model_app_key != previous.model_app_key
        || current.model_app_secret != previous.model_app_secret
        || current.base_url != previous.base_url
    {
        return Err(GatewayError::with_status(409, "套餐检查期间账号凭证已更换，未覆盖新账号"));
    }
    let expected = crate::store::secret_view(&record);
    let next = crate::store::with_fields(&expected, &[
        ("baseUrl", Value::String(base_url.to_string())),
        ("modelAppKey", Value::String(app_key.to_string())),
        ("modelAppSecret", Value::String(app_secret.to_string())),
    ]);
    if next == expected || crate::store::save(ctx, account_id, &expected, &next).await? {
        return Ok(());
    }
    let latest = crate::store::record(ctx, super::ID, account_id).await?;
    let latest = credentials::from_record(Some(&latest))?;
    if latest.base_url == base_url && latest.model_app_key == app_key && latest.model_app_secret == app_secret {
        Ok(())
    } else {
        Err(GatewayError::with_status(409, "免费套餐开通后的模型凭证未能保存，请重新登录同步；账号已保留"))
    }
}

fn report(status: &str, message: &str) -> Value {
    json!({ "status": status, "message": message })
}

fn redact(message: &mut String, value: &Value) {
    match value {
        Value::Object(fields) => {
            for value in fields.values() { redact(message, value); }
        }
        Value::Array(values) => {
            for value in values { redact(message, value); }
        }
        Value::String(value) if value.len() >= 16 => {
            *message = message.replace(value, "[已隐藏]");
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_marks_and_clears() {
        let account_id = "officeace-pending-mark-test";
        set_pending(account_id, true);
        assert!(pending(account_id));
        set_pending(account_id, false);
        assert!(!pending(account_id));
    }

    #[test]
    fn pending_falls_back_after_timeout() {
        let account_id = "officeace-pending-timeout-test";
        pending_accounts()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(account_id.to_string(), crate::logging::now_ms() - PENDING_TTL_MS - 1000);
        assert!(!pending(account_id));
    }
}
