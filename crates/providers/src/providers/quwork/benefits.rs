use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use reqwest::Method;
use serde_json::{json, Value};

use super::{accounts, checkin, client, desktop, endpoints, refresh};
use crate::errors::GatewayError;

fn daily_view(status: &checkin::Status, saved: &str) -> Value {
    json!({
        "campaignId": format!("daily:{}", status.reset_at),
        "title": "QuWork 每日奖励", "titleEn": "QuWork daily reward",
        "description": "与每日签到共用领取状态，不会重复领取。",
        "descriptionEn": "Shares its claim state with daily check-in; rewards are never claimed twice.",
        "amount": status.points, "unit": "积分",
        "status": if status.claimed || saved == "claimed" { "claimed" } else if saved == "confirming" { "confirming" }
            else if status.can_claim { "claimable" } else { "ineligible" },
        "startsAt": Value::Null, "endsAt": status.reset_at,
        "claimedAt": Value::Null, "expiresAt": Value::Null, "validDays": Value::Null,
    })
}

pub async fn preview(ctx: &PluginCtx, account: &str) -> Result<Value, String> {
    let result = async {
        let secret = refresh::snapshot(ctx, account).await?;
        if accounts::text(&secret, &["accessToken"]).is_empty() {
            return Err(crate::errors::usage_not_configured(
                "QuWork",
                "登录凭证 accessToken",
            ));
        }
        let mut notices = Vec::new();
        let campaigns = match checkin::status(ctx, account).await {
            Ok(status) => {
                let saved = checkin::saved_state(ctx, account, &status).await?;
                vec![daily_view(&status, saved)]
            }
            Err(error) => {
                notices.push(error.message);
                Vec::new()
            }
        };
        let monthly = refresh::protected(
            ctx,
            account,
            Method::GET,
            endpoints::MONTHLY_GIFTS_PATH,
            None,
        )
        .await
        .and_then(|reply| reply.data("月度赠送查询"))
        .and_then(|data| monthly_rows(&data));
        let monthly_gifts = match monthly {
            Ok(rows) => Some(rows),
            Err(error) => {
                notices.push(error.message);
                None
            }
        };
        Ok(json!({
            "campaigns": campaigns, "riskNote": Value::Null,
            "monthlyGifts": monthly_gifts, "notice": notices.join("；"),
            "redemption": { "supported": true },
        }))
    }
    .await;
    result.map_err(GatewayError::into_control_message)
}

fn monthly_rows(data: &Value) -> Result<Vec<Value>, GatewayError> {
    if !data.is_object() {
        return Err(GatewayError::with_status(
            502,
            "QuWork 月度赠送记录格式异常",
        ));
    }
    let mut rows = Vec::new();
    let Some(list) = data.get("list").filter(|value| !value.is_null()) else {
        return Ok(rows);
    };
    let orders = list
        .as_array()
        .filter(|orders| orders.len() <= 100)
        .ok_or_else(|| GatewayError::with_status(502, "QuWork 月度赠送记录格式异常"))?;
    for order in orders {
        let id = order
            .get("order_id")
            .and_then(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| value.as_u64().map(|value| value.to_string()))
            })
            .unwrap_or_default();
        for (index, period) in order
            .get("periods")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(120)
            .enumerate()
        {
            let status = client::number(period.get("gift_status")).map(|value| value as i64);
            rows.push(json!({
                "id": format!("{id}:{index}"), "orderId": id, "period": index + 1,
                "amount": client::number(period.get("points")).or_else(|| client::number(period.get("quota"))),
                "scheduledAt": client::timestamp(period.get("expected_gift_date")),
                "grantedAt": client::timestamp(period.get("actual_gift_date")),
                "status": match status { Some(1) => "pending", Some(2) => "granted", Some(3) => "failed",
                    Some(4) => "cancelled", Some(5) => "processing", _ => "unknown" },
            }));
        }
    }
    Ok(rows)
}

pub async fn claim(ctx: &PluginCtx, account: &str, id: &str) -> Result<Value, String> {
    let status = checkin::status(ctx, account)
        .await
        .map_err(GatewayError::into_control_message)?;
    if id != format!("daily:{}", status.reset_at) {
        return Ok(json!({ "campaignId": id, "outcome": "inactive", "campaign": Value::Null }));
    }
    let result = checkin::run(ctx, account).await?;
    let outcome = match result.outcome {
        proto::CheckinOutcome::Claimed => "claimed",
        proto::CheckinOutcome::Done => "already_claimed",
        proto::CheckinOutcome::Ineligible => "ineligible",
        proto::CheckinOutcome::Inactive => "inactive",
        _ => "failed",
    };
    let latest = checkin::status(ctx, account).await.ok();
    let status = latest.as_ref().unwrap_or(&status);
    let saved = checkin::saved_state(ctx, account, status)
        .await
        .map_err(GatewayError::into_control_message)?;
    Ok(
        json!({ "campaignId": id, "outcome": if saved == "confirming" && !status.claimed { "confirming" } else { outcome },
        "campaign": daily_view(status, saved), "message": result.message }),
    )
}

pub async fn redeem(ctx: &PluginCtx, account: &str, payload: &Value) -> Result<Value, String> {
    let code = accounts::text(payload, &["code"]);
    if code.is_empty() || code.len() > 32 || code.chars().any(char::is_control) {
        return Err("请填写 QuWork 权益兑换码或 16 位订单号".into());
    }
    let stored = ctx
        .store_get(endpoints::PROVIDER, account)
        .await
        .map_err(|error| error.to_string())?;
    crate::flight::run(&format!("quwork-redeem:{}", stored.id), || async {
        redeem_locked(ctx, &stored.id, &code)
            .await
            .map_err(GatewayError::into_control_message)
    })
    .await
}

async fn redeem_locked(ctx: &PluginCtx, account: &str, code: &str) -> Result<Value, GatewayError> {
    let mut secret = refresh::snapshot(ctx, account).await?;
    if refresh::expiring(&secret) {
        secret = refresh::refresh(ctx, account, false).await?;
    }
    let access = accounts::text(&secret, &["accessToken"]);
    if access.is_empty() {
        return Err(GatewayError::with_status(
            400,
            "QuWork 权益兑换需要完整登录态",
        ));
    }
    let order = code.len() == 16 && code.bytes().all(|byte| byte.is_ascii_digit());
    let device = if order {
        String::new()
    } else {
        let configured = accounts::text(&secret, &["deviceIdentifier"]);
        if configured.is_empty() {
            desktop::device_identifier().await?
        } else {
            configured
        }
    };
    let key = format!("quworkRedemption:{}", crate::flight::fingerprint(code));
    let previous = ctx
        .ledger_get(account, &key)
        .await
        .map_err(|_| GatewayError::with_status(500, "QuWork 兑换记录读取失败，未提交兑换"))?;
    match previous.get("state").and_then(Value::as_str) {
        Some("succeeded") => return Ok(json!({ "outcome": "already_claimed" })),
        Some("confirming") => return Ok(json!({ "outcome": "confirming" })),
        _ => {}
    }
    ctx.ledger_put(
        account,
        &key,
        &json!({ "state": "confirming", "at": crate::http::now_ms() }),
    )
    .await
    .map_err(|_| GatewayError::with_status(500, "QuWork 兑换记录保存失败，未提交兑换"))?;
    let body = if order {
        json!({ "order_id": code })
    } else {
        json!({
            "invite_code": code, "device_identifier": device,
            "device_uuid": accounts::text(&secret, &["deviceUuid"]),
        })
    };
    let response = client::request(
        &client::client(ctx, None),
        Method::POST,
        if order {
            endpoints::REDEEM_PATH
        } else {
            endpoints::ACTIVATE_PATH
        },
        &access,
        Some(&body),
    )
    .await;
    let (state, outcome, message) = match response {
        Ok(reply) if reply.ok() => ("succeeded", "claimed", String::new()),
        Ok(reply)
            if (400..500).contains(&reply.status) && reply.status != 408
                || ((200..300).contains(&reply.status)
                    && reply.body.get("success").is_some()
                    && !client::success(reply.body.get("success"))) =>
        {
            let message = client::message(&reply.body).replace(code, "[已隐藏]");
            ("rejected", "failed", message)
        }
        _ => ("confirming", "confirming", String::new()),
    };
    let _ = ctx
        .ledger_put(
            account,
            &key,
            &json!({ "state": state, "at": crate::http::now_ms() }),
        )
        .await;
    Ok(json!({ "outcome": outcome, "message": message }))
}
