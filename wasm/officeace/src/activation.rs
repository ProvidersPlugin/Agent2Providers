//! 零元体验开通是明确管理动作，导入/登录/签到不代办同意与下单。
//! 用户确认、官方零价、身份与模型凭据绑定、提交前台账、不续费确认五道检查全部保留。

use super::{
    account, activation_state as state, agreement, client, control::Context,
    credentials::Credentials, endpoints, errors, oauth_gateway, trial, util,
};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use state::{report, view};

pub fn preview() -> Result<Value> {
    let (record, credential) = account::snapshot()?;
    let result = preview_inner(&credential).map_err(|mut error| {
        errors::redact(&mut error, &record["secret"]);
        error
    })?;
    account::unchanged(&record)?;
    Ok(result)
}

fn preview_inner(credential: &Credentials) -> Result<Value> {
    if credential.control().is_err() {
        return Ok(view(
            "credentials_missing",
            "缺少套餐开通凭据，请通过果办网页登录补齐授权",
        ));
    }
    if endpoints::official_base(&credential.base_url).is_err() {
        return Ok(view(
            "unsupported",
            "未使用官方果办网关，无法在此开通免费套餐",
        ));
    }
    if credential.expires_at > 0 && credential.expires_at <= sdk::now() {
        return Ok(view(
            "credentials_expired",
            "套餐查询凭据已过期，请先刷新凭据；此预览不会自动续期",
        ));
    }
    let snapshot = match client::gateway_control(credential, "GET", "/v1/subscription", None, true)
    {
        Ok(value) => value,
        Err(error) if error.code.as_deref() == Some(errors::CONTROL_EXPIRED) => {
            return Ok(view("credentials_expired", &error.message))
        }
        Err(error) if matches!(error.status, Some(401 | 403)) => {
            return Ok(view("permission_denied", &error.message))
        }
        Err(error) => return Err(error),
    };
    let ledger = state::read()?;
    let own = state::owns(&ledger, credential);
    match snapshot["subscribe_status"].as_str() {
        Some("SUBSCRIBED") => {
            let mut result = view("active", "套餐已开通，无需重复操作");
            if own
                || snapshot["skus"]
                    .as_array()
                    .is_some_and(|rows| rows.iter().any(|row| row["sku_code"] == trial::SKU))
            {
                if let Ok(rows) = trial::readonly(credential) {
                    renewal(&mut result, &rows);
                }
            }
            Ok(result)
        }
        Some("UNSUBSCRIBED") => {
            let rows = trial::readonly(credential)?;
            if own {
                if let Some(result) = state::pending(&ledger, &rows) {
                    return Ok(result);
                }
            }
            if trial::has_order(&rows) {
                return Ok(view(
                    "unknown",
                    "官方存在未完成套餐订单，请在官方客户端核对；这里不会重复下单",
                ));
            }
            if !rows.is_empty() {
                return Ok(view(
                    "expired",
                    "官方已有历史套餐记录，这里不会重复购买或续费",
                ));
            }
            Ok(view(
                "inactive",
                "尚未开通套餐，可确认服务协议后开通零元体验套餐，不自动续费",
            ))
        }
        Some("EXPIRED") => Ok(view(
            "expired",
            "原套餐已到期，请在官方客户端处理；这里不会自动续费",
        )),
        Some("FROZEN") => Ok(view(
            "frozen",
            "原套餐已冻结，请在官方客户端处理；这里不会自动变更套餐",
        )),
        _ => Ok(view("unknown", "官方未返回明确套餐状态，暂不能开通")),
    }
}

pub fn ensure(consent: bool) -> Result<Value> {
    let (before, _) = account::snapshot()?;
    let result = ensure_inner(consent);
    result.map_err(|mut error| {
        errors::redact(&mut error, &before["secret"]);
        if let Ok((latest, _)) = account::snapshot() {
            errors::redact(&mut error, &latest["secret"]);
        }
        error
    })
}

fn ensure_inner(consent: bool) -> Result<Value> {
    let (_, initial) = account::snapshot()?;
    if initial.control().is_err() {
        return Ok(report("skipped", "缺少套餐开通凭据，请网页登录补齐授权"));
    }
    endpoints::official_base(&initial.base_url)?;
    let mut context = Context::acquire()?;
    endpoints::official_base(&context.credential.base_url)?;
    let before = context.subscription()?;
    match before["subscribe_status"].as_str() {
        Some("SUBSCRIBED") => {
            let mut result = report("already_active", "套餐已开通，无需重复操作");
            if let Ok(rows) = trial::resources(&mut context) {
                renewal(&mut result, &rows);
            }
            return Ok(result);
        }
        Some("UNSUBSCRIBED") => {}
        Some("EXPIRED" | "FROZEN") => {
            return Ok(report(
                "skipped",
                "原套餐已到期或冻结，未自动变更或续费；请在官方客户端处理",
            ))
        }
        _ => return Ok(report("skipped", "官方未返回明确套餐状态，未自动开通")),
    }
    if !consent {
        return Ok(report(
            "skipped",
            "请先在签到中心明确确认服务协议；导入账号不会自动同意协议或下单",
        ));
    }
    let ledger = state::read()?;
    context.resolve_project()?;
    let rows = trial::resources(&mut context)?;
    if state::owns(&ledger, &context.credential) {
        if let Some(result) = state::pending(&ledger, &rows) {
            return Ok(report(
                if result["status"] == "pending" {
                    "pending"
                } else {
                    "skipped"
                },
                result["message"].as_str().unwrap_or("请核对已有订单"),
            ));
        }
    }
    if !rows.is_empty() {
        return Ok(report(
            "skipped",
            "官方已有套餐或历史订单，请先核对；这里不会续费或重复购买",
        ));
    }
    verify_identity(&context)?;
    // 在任何协议写入之前就确认零价；写协议之后再验一次，防止价格/销售周期改变。
    trial::zero_order(&mut context)?;
    agreement::confirm(&mut context)?;
    let body = trial::zero_order(&mut context)?;
    let latest = context.subscription()?;
    if latest["subscribe_status"] == "SUBSCRIBED" {
        return Ok(report("already_active", "套餐已开通，无需重复操作"));
    }
    if latest["subscribe_status"] != "UNSUBSCRIBED" || !trial::resources(&mut context)?.is_empty() {
        return Ok(report(
            "skipped",
            "套餐或订单状态已变化，未提交新订单，请重新查询",
        ));
    }
    context.verify()?;
    let mut attempt = state::submitted(&context.credential);
    state::write(&attempt)?;
    match context.cloud("POST", "/v1/subscription/period", Some(&body)) {
        Ok(value) => {
            attempt["status"] = json!("submitted");
            for key in ["order_id", "cbc_order_id"] {
                if let Some(value) = value[key].as_str() {
                    attempt[key] = json!(value);
                }
            }
            state::write(&attempt)?;
        }
        Err(error) if error.code.as_deref() == Some(errors::UNCERTAIN) => {
            return Ok(report(
                "pending",
                "零元套餐请求结果未确认，请稍后查询；不会自动重复下单",
            ));
        }
        Err(error) => {
            attempt["status"] = json!("rejected");
            state::write(&attempt)?;
            return Err(error);
        }
    }
    for index in 0..4 {
        if index > 0 {
            sdk::sleep(2000)?;
        }
        let snapshot = match context.subscription() {
            Ok(value) => value,
            Err(_) => break,
        };
        if snapshot["subscribe_status"] != "SUBSCRIBED" {
            continue;
        }
        let rows = match trial::resources(&mut context) {
            Ok(rows) => rows,
            Err(_) => break,
        };
        if let Some(no_renew) = trial::no_renew(&rows) {
            attempt["status"] = json!("active");
            state::write(&attempt)?;
            let mut result = report(
                "activated",
                if no_renew {
                    "零元体验套餐已开通并确认生效，自动续费已关闭"
                } else {
                    "套餐已生效，但官方返回自动续费已开启，与提交参数不符；请立即到官方客户端检查"
                },
            );
            result["autoRenew"] = json!(!no_renew);
            return Ok(result);
        }
    }
    Ok(report(
        "pending",
        "零元套餐订单已提交，正在等待套餐和不续费状态确认，请稍后查询",
    ))
}

fn verify_identity(context: &Context) -> Result<()> {
    let permission = oauth_gateway::permission(&context.credential)?;
    for (key, actual) in [
        ("accountId", &permission.account),
        ("principalId", &permission.principal),
    ] {
        let expected = util::text(&context.record["secret"], &[key]);
        if actual.is_empty() || !expected.is_empty() && expected != *actual {
            return Err(Error::http(409, "果办登录身份与账号记录不一致，未开通套餐"));
        }
    }
    let mut secret = json!({});
    oauth_gateway::apply_model(&mut secret, &permission.model)?;
    let actual = Credentials::from_secret(&secret);
    if endpoints::normalize_base(&actual.base_url)
        != endpoints::normalize_base(&context.credential.base_url)
        || actual.model_app_key != context.credential.model_app_key
        || actual.model_app_secret != context.credential.model_app_secret
    {
        return Err(Error::http(409, "果办模型凭据与登录身份不一致，未开通套餐"));
    }
    Ok(())
}

fn renewal(value: &mut Value, rows: &[Value]) {
    match trial::no_renew(rows) {
        Some(true) => {
            value["autoRenew"] = json!(false);
            value["message"] = json!("体验套餐已生效，自动续费已关闭");
        }
        Some(false) => {
            value["autoRenew"] = json!(true);
            value["message"] = json!("套餐已生效，但官方返回自动续费已开启，请到官方客户端检查");
        }
        None => value["message"] = json!("套餐已生效，不续费状态尚待官方同步，请稍后查询确认"),
    }
}
