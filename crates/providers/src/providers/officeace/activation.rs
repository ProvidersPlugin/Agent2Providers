use std::time::Duration;

use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use crate::errors::GatewayError;
use super::{activation_state as state, agreement, control, credentials, oauth, subscription, trial, upstream};
use credentials::OfficeAceCredential;
use state::{report, view};

pub async fn preview(ctx: &PluginCtx, account_id: &str) -> Result<Value, String> {
    if account_id.is_empty() {
        return Err(GatewayError::with_status(400, "免费套餐检查缺少账号").into_control_message());
    }
    let record = crate::store::record(ctx, super::ID, account_id).await
        .map_err(GatewayError::into_control_message)?;
    preview_inner(ctx, account_id, &record).await.map_err(|mut error| {
        redact(&mut error.message, &record);
        error.into_control_message()
    })
}

async fn preview_inner(ctx: &PluginCtx, account_id: &str, record: &Value) -> Result<Value, GatewayError> {
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
    let snapshot = match subscription::fetch_subscription(&credential).await {
        Ok(snapshot) => snapshot,
        Err(error) if error.code.as_deref() == Some(subscription::CONTROL_TOKEN_EXPIRED) => {
            return Ok(view("credentials_expired", &error.message));
        }
        Err(error) if matches!(error.status_code, 401 | 403) => {
            return Ok(view("permission_denied", &error.message));
        }
        Err(error) => return Err(error),
    };
    let status = snapshot.get("subscribe_status").and_then(Value::as_str);
    let ledger = state::read(ctx, account_id).await?;
    let own_ledger = state::for_credential(&ledger, &credential);
    if status == Some("SUBSCRIBED") {
        let mut result = view("active", "套餐已开通，无需重复操作");
        if own_ledger || snapshot.get("skus").and_then(Value::as_array).is_some_and(|rows| {
            rows.iter().any(|row| row.get("sku_code").and_then(Value::as_str) == Some(trial::SKU))
        }) {
            if let Ok(rows) = trial::resources_readonly(&credential).await {
                apply_renewal(&mut result, &rows);
            }
        }
        return Ok(result);
    }
    if status == Some("UNSUBSCRIBED") {
        let rows = trial::resources_readonly(&credential).await?;
        if own_ledger {
            if let Some(result) = state::pending_view(&ledger, &rows) { return Ok(result); }
        }
        if trial::has_open_order(&rows) {
            return Ok(view("unknown", "官方存在未完成或正在生效的套餐订单，请到官方客户端核对；这里不会重复下单"));
        }
        if !rows.is_empty() {
            return Ok(view("expired", "官方已有历史套餐记录，这里不会重复购买或续费；请在官方客户端处理"));
        }
        return Ok(view("inactive", "尚未开通套餐，可确认服务协议后开通零元体验套餐，不自动续费"));
    }
    Ok(match status {
        Some("EXPIRED") => view("expired", "原套餐已到期，请在官方客户端处理；这里不会自动续费或变更套餐"),
        Some("FROZEN") => view("frozen", "原套餐已冻结，请在官方客户端处理；这里不会自动续费或变更套餐"),
        _ => view("unknown", "官方未返回明确的套餐状态，暂不能开通"),
    })
}

pub async fn ensure(ctx: &PluginCtx, account_id: &str, consent: bool) -> Result<Value, String> {
    if account_id.is_empty() {
        return Err(GatewayError::with_status(400, "免费套餐检查缺少账号").into_control_message());
    }
    crate::flight::run(&format!("officeace-subscription:{account_id}"), || async {
        let record = crate::store::record(ctx, super::ID, account_id).await
            .map_err(GatewayError::into_control_message)?;
        match ensure_inner(ctx, account_id, &record, consent).await {
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
    consent: bool,
) -> Result<Value, GatewayError> {
    let initial = credentials::from_record(Some(record))?;
    if !initial.has_control_plane() || initial.security_token.is_empty() {
        return Ok(report("skipped", "缺少套餐开通凭证，请通过果办网页登录补齐授权"));
    }
    official_gateway(&initial.base_url)?;
    let mut credential = control::credential_of(ctx, account_id).await?;
    official_gateway(&credential.base_url)?;
    let before = control::subscription_of(ctx, &mut credential).await?;
    match before.get("subscribe_status").and_then(Value::as_str) {
        Some("SUBSCRIBED") => {
            let mut result = report("already_active", "套餐已开通，无需重复操作");
            if let Ok(rows) = trial::resources(ctx, &mut credential).await {
                apply_renewal(&mut result, &rows);
            }
            return Ok(result);
        }
        Some("UNSUBSCRIBED") => {}
        Some("EXPIRED" | "FROZEN") => return Ok(report("skipped", "原套餐已到期或冻结，未自动变更或续费；请在官方客户端检查套餐")),
        _ => return Ok(report("skipped", "官方未返回明确的套餐状态，未自动开通")),
    }
    if !consent {
        return Ok(report("skipped", "尚未开通体验套餐，请到签到中心确认服务协议后开通；导入账号不会自动同意协议或下单"));
    }
    let ledger = state::read(ctx, account_id).await?;
    control::resolve_project(ctx, &mut credential).await?;
    let rows = trial::resources(ctx, &mut credential).await?;
    if state::for_credential(&ledger, &credential) {
        if let Some(result) = state::pending_view(&ledger, &rows) {
            return Ok(report(if result["status"] == "pending" { "pending" } else { "skipped" },
                result["message"].as_str().unwrap_or("请先核对已有订单")));
        }
    }
    if !rows.is_empty() {
        return Ok(report("skipped", "官方已有套餐或历史订单，请在官方客户端核对；这里不会续费或重复购买"));
    }
    verify_identity(&credential, record).await?;
    // 协议确认和零元订单都只在用户明确同意后执行；可选的数据改进授权不在此流程中。
    trial::zero_price_order(ctx, &mut credential).await?;
    agreement::confirm(ctx, &mut credential).await?;
    let body = trial::zero_price_order(ctx, &mut credential).await?;
    let latest = control::subscription_of(ctx, &mut credential).await?;
    if latest.get("subscribe_status").and_then(Value::as_str) == Some("SUBSCRIBED") {
        return Ok(report("already_active", "套餐已开通，无需重复操作"));
    }
    if latest.get("subscribe_status").and_then(Value::as_str) != Some("UNSUBSCRIBED")
        || !trial::resources(ctx, &mut credential).await?.is_empty()
    {
        return Ok(report("skipped", "套餐或订单状态已变化，未提交新订单，请重新查询"));
    }
    // 提交前落盘；即使插件重启或调用方超时，也不会遗忘可能已到达上游的订单。
    let mut attempt = state::submitted(&credential);
    state::write(ctx, account_id, &attempt).await?;
    let order = control::cloud_call(ctx, &mut credential, "POST", "/v1/subscription/period", Some(body)).await;
    match order {
        Ok(value) => {
            attempt["status"] = json!("submitted");
            for key in ["order_id", "cbc_order_id"] {
                if let Some(value) = value.get(key).and_then(Value::as_str) {
                    attempt[key] = json!(value);
                }
            }
            state::write(ctx, account_id, &attempt).await?;
        }
        Err(error) if error.code.as_deref() == Some(upstream::RESULT_UNCERTAIN) => {
            return Ok(report("pending", "零元套餐请求结果暂未确认，请稍后查询；不会自动重复下单"));
        }
        Err(error) => {
            attempt["status"] = json!("rejected");
            state::write(ctx, account_id, &attempt).await?;
            return Err(error);
        }
    }
    for index in 0..4 {
        if index > 0 { tokio::time::sleep(Duration::from_secs(2)).await; }
        let snapshot = match control::subscription_of(ctx, &mut credential).await {
            Ok(value) => value,
            Err(_) => break,
        };
        if snapshot.get("subscribe_status").and_then(Value::as_str) != Some("SUBSCRIBED") { continue; }
        let resources = match trial::resources(ctx, &mut credential).await {
            Ok(value) => value,
            Err(_) => break,
        };
        let mut result = report("activated", "零元体验套餐已开通并确认生效，自动续费已关闭");
        match trial::no_auto_renew(&resources) {
            Some(true) => {
                attempt["status"] = json!("active");
                state::write(ctx, account_id, &attempt).await?;
                result["autoRenew"] = json!(false);
                return Ok(result);
            }
            Some(false) => {
                attempt["status"] = json!("active");
                state::write(ctx, account_id, &attempt).await?;
                result["message"] = json!("套餐已生效，但官方返回自动续费已开启，与提交参数不符；请立即在官方客户端检查");
                result["autoRenew"] = json!(true);
                return Ok(result);
            }
            None => {}
        }
    }
    Ok(report("pending", "零元套餐订单已提交，正在等待套餐和不续费状态确认，请稍后查询"))
}

async fn verify_identity(credential: &OfficeAceCredential, record: &Value) -> Result<(), GatewayError> {
    let signing = credential.signing().map_err(GatewayError::from)?;
    let (account, principal, model_info) = oauth::fetch_permission(&signing).await
        .map_err(|message| GatewayError::with_status(502, message))?;
    for (key, actual) in [("accountId", account.as_str()), ("principalId", principal.as_str())] {
        let expected = crate::store::pick(record, &[key]);
        if actual.is_empty() || !expected.is_empty() && expected != actual {
            return Err(GatewayError::with_status(409, "果办登录身份与账号记录不一致，未开通套餐"));
        }
    }
    let (base, key, secret) = oauth::gateway_of(&model_info).map_err(GatewayError::from)?;
    if super::chat::normalize_base(&base) != super::chat::normalize_base(&credential.base_url)
        || key != credential.model_app_key || secret != credential.model_app_secret
    {
        return Err(GatewayError::with_status(409, "果办模型凭证与登录身份不一致，未开通套餐"));
    }
    Ok(())
}

fn apply_renewal(result: &mut Value, rows: &[Value]) {
    match trial::no_auto_renew(rows) {
        Some(true) => {
            result["autoRenew"] = json!(false);
            result["message"] = json!("体验套餐已生效，自动续费已关闭");
        }
        Some(false) => {
            result["autoRenew"] = json!(true);
            result["message"] = json!("套餐已生效，但官方返回自动续费已开启，请到官方客户端检查");
        }
        None => {
            result["message"] = json!("套餐已生效，不续费状态尚待官方同步，请稍后查询确认");
        }
    }
}

fn official_gateway(base_url: &str) -> Result<(), GatewayError> {
    let valid = url::Url::parse(base_url).is_ok_and(|url| {
        url.scheme() == "https" && url.username().is_empty() && url.password().is_none()
            && url.port_or_known_default() == Some(443) && url.query().is_none() && url.fragment().is_none()
            && matches!(url.path().trim_end_matches('/'), "" | "/v2")
            && url.host_str().is_some_and(|host| host.ends_with(".officeace.cn-southwest-2.huaweicloud-agentarts.com"))
    });
    if valid { Ok(()) } else {
        Err(GatewayError::with_status(400, "账号未使用官方果办网关地址，未开通免费套餐"))
    }
}

fn redact(message: &mut String, value: &Value) {
    match value {
        Value::Object(fields) => for value in fields.values() { redact(message, value); },
        Value::Array(values) => for value in values { redact(message, value); },
        Value::String(value) if value.len() >= 16 => *message = message.replace(value, "[已隐藏]"),
        _ => {}
    }
}
