//! 手动活动福利：只领上游明确下发的非每日 Credits 活动，不触碰付费促销。
//! 名称取自官方活动页使用的 placements[].content.zh/en，不固定活动编号或奖励额。

use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use crate::errors::GatewayError;
use crate::http::now_ms;

use super::campaigns::{self, campaign_text, campaign_time, CampaignSession};
use super::endpoints::Site;
use super::refresh;

const LEDGER_KIND: &str = "qoderCampaignClaims";

fn is_reward(row: &Value) -> bool {
    campaigns::is_credit_benefit(row) && !campaigns::is_daily(row)
}

fn rows(list: &Value) -> impl Iterator<Item = &Value> {
    list.get("campaigns").and_then(Value::as_array).into_iter().flatten().filter(|row| is_reward(row))
}

fn status(row: &Value) -> &'static str {
    if row.get("claimStatus").and_then(Value::as_str) == Some("CLAIMED") { return "claimed"; }
    let now = now_ms();
    if campaign_time(row.get("startAt")).is_some_and(|start| start > now)
        || campaign_time(row.get("endAt")).is_some_and(|end| end <= now) {
        return "inactive";
    }
    if row.get("claimStatus").and_then(Value::as_str) == Some("CLAIMABLE") { "claimable" } else { "ineligible" }
}

fn view(row: &Value) -> Value {
    let title = campaign_text(row, "zh", "title");
    let title_en = campaign_text(row, "en", "title");
    let fallback = row.get("campaignKey").and_then(Value::as_str).unwrap_or("Qoder");
    json!({
        "campaignId": row.get("campaignId"),
        "title": if title.is_empty() { if title_en.is_empty() { fallback } else { &title_en } } else { &title },
        "titleEn": title_en,
        "description": campaign_text(row, "zh", "description"),
        "descriptionEn": campaign_text(row, "en", "description"),
        "amount": row.pointer("/benefit/amount").and_then(Value::as_f64).filter(|amount| *amount >= 0.0),
        "status": status(row),
        "startsAt": campaign_time(row.get("startAt")),
        "endsAt": campaign_time(row.get("endAt")),
        "claimedAt": campaign_time(row.get("claimedAt")),
        "expiresAt": campaign_time(row.get("expiresAt")).or_else(|| campaign_time(row.pointer("/benefit/validity/fixedEnd"))),
        "validDays": row.pointer("/benefit/validity/days").and_then(Value::as_f64).filter(|days| *days > 0.0),
    })
}

async fn ledger(ctx: &PluginCtx, account: &str, session: &CampaignSession) -> Result<Value, GatewayError> {
    let mut value = ctx.ledger_get(account, LEDGER_KIND).await
        .map_err(|_| GatewayError::with_status(500, "无法读取活动领取记录，未提交领取"))?;
    let identity = format!("{}:{}", session.credentials.site.provider_id(), session.credentials.user_id);
    if value.get("identity").and_then(Value::as_str) != Some(identity.as_str()) {
        value = json!({ "identity": identity, "claims": {} });
    }
    if !value.get("claims").is_some_and(Value::is_object) { value["claims"] = json!({}); }
    if let Some(claims) = value.get_mut("claims").and_then(Value::as_object_mut) {
        claims.retain(|_, claim| claim.get("at").and_then(Value::as_i64)
            .is_some_and(|at| now_ms().saturating_sub(at) < 90 * 86_400_000));
    }
    Ok(value)
}

fn apply_saved(mut campaign: Value, saved: Option<&Value>) -> Value {
    if let Some(saved) = saved.filter(|saved| saved.get("startsAt") == campaign.get("startsAt")
        && saved.get("endsAt") == campaign.get("endsAt")) {
        if campaign.get("status").and_then(Value::as_str) == Some("claimable") {
            if let Some(saved_status) = saved.get("status").and_then(Value::as_str)
                .filter(|value| matches!(*value, "claimed" | "confirming")) {
                campaign["status"] = json!(saved_status);
            }
        }
        for field in ["claimedAt", "expiresAt"] {
            if let Some(value) = saved.get(field).filter(|value| !value.is_null()) {
                campaign[field] = value.clone();
            }
        }
    }
    campaign
}

pub(super) async fn preview(ctx: &PluginCtx, site: Site, account: &str) -> Result<Value, GatewayError> {
    let session = campaigns::load(ctx, site, account).await?;
    let saved = ledger(ctx, account, &session).await?;
    let campaigns: Vec<Value> = rows(&session.list).map(|row| {
        let id = row.get("campaignId").and_then(Value::as_str).unwrap_or("");
        apply_saved(view(row), saved.get("claims").and_then(|claims| claims.get(id)))
    }).collect();
    Ok(json!({ "campaigns": campaigns, "riskNote": session.risk_note }))
}

pub(super) async fn claim(ctx: &PluginCtx, site: Site, account: &str, id: &str) -> Result<Value, String> {
    let id = id.trim();
    if id.is_empty() || id.len() > 256 || id == "." || id == ".." || id.chars().any(char::is_control) {
        return Err(GatewayError::with_status(400, "请选择要领取的活动").into_control_message());
    }
    let key = format!("qoder-campaigns:{}:{account}", site.provider_id());
    crate::flight::run(&key, || async {
        claim_locked(ctx, site, account, id).await.map_err(GatewayError::into_control_message)
    }).await
}

async fn claim_locked(ctx: &PluginCtx, site: Site, account: &str, id: &str) -> Result<Value, GatewayError> {
    let mut session = campaigns::load(ctx, site, account).await?;
    let mut saved = ledger(ctx, account, &session).await?;
    let Some(target) = rows(&session.list).find(|row| row.get("campaignId").and_then(Value::as_str) == Some(id)).cloned() else {
        return Ok(result(id, "inactive", None));
    };
    let campaign = apply_saved(view(&target), saved.get("claims").and_then(|claims| claims.get(id)));
    match campaign.get("status").and_then(Value::as_str) {
        Some("claimed") => return Ok(result(id, "already_claimed", Some(campaign))),
        Some("confirming") => return Ok(result(id, "confirming", Some(campaign))),
        Some("inactive") => return Ok(result(id, "inactive", Some(campaign))),
        Some("ineligible") => return Ok(result(id, "ineligible", Some(campaign))),
        _ => {}
    }
    if saved["claims"].as_object().is_some_and(|claims| claims.len() >= 256) {
        return Err(GatewayError::with_status(409, "活动领取记录已满，请稍后再试"));
    }
    // 写前持久化：请求超时、页面关闭或进程重启后只能回查，不能盲目再领一次。
    saved["claims"][id] = json!({
        "at": now_ms(), "status": "confirming",
        "startsAt": campaign.get("startsAt"), "endsAt": campaign.get("endsAt"),
    });
    ctx.ledger_put(account, LEDGER_KIND, &saved).await
        .map_err(|_| GatewayError::with_status(500, "无法保存活动领取记录，未提交领取"))?;

    let mut response = campaigns::submit(&session, id).await;
    if response.as_ref().is_ok_and(|response| response.status == 401) {
        let fresh = async {
            refresh::ensure_fresh(ctx, site, account, true).await?;
            campaigns::load(ctx, site, account).await
        }.await;
        session = match fresh {
            Ok(fresh) => fresh,
            Err(error) => {
                finish(ctx, account, id, saved, "failed", campaign).await?;
                return Err(error);
            }
        };
        let target = rows(&session.list).find(|row| row.get("campaignId").and_then(Value::as_str) == Some(id)).cloned();
        match target {
            Some(row) if status(&row) == "claimed" => {
                return finish(ctx, account, id, saved, "already_claimed", view(&row)).await;
            }
            Some(row) if status(&row) == "claimable" => response = campaigns::submit(&session, id).await,
            Some(row) => return finish(ctx, account, id, saved, status(&row), view(&row)).await,
            None => return finish(ctx, account, id, saved, "inactive", campaign).await,
        }
    }
    let mut rejected = false;
    if let Ok(response) = &response {
        let body = campaigns::unwrap_data(response.payload.clone().unwrap_or(Value::Null));
        let already = body.get("result").and_then(Value::as_str) == Some("ALREADY_CLAIMED");
        if (response.ok || response.status == 409) && already {
            return finish(ctx, account, id, saved, "already_claimed", receipt(campaign, &body)).await;
        }
        if response.ok && body.get("status").and_then(Value::as_str) == Some("CLAIMED") {
            let outcome = if body.get("replayed").and_then(Value::as_bool) == Some(true) { "already_claimed" } else { "claimed" };
            return finish(ctx, account, id, saved, outcome, receipt(campaign, &body)).await;
        }
        rejected = matches!(response.status, 400..=499) && response.status != 408;
    }

    if let Ok(list) = campaigns::fetch(&session.credentials, session.proxy.as_ref(), &session.headers).await {
        if let Some(row) = rows(&list).find(|row| row.get("campaignId").and_then(Value::as_str) == Some(id)) {
            if status(row) == "claimed" {
                return finish(ctx, account, id, saved, "already_claimed", view(row)).await;
            }
            if rejected && status(row) != "claimable" {
                return finish(ctx, account, id, saved, status(row), view(row)).await;
            }
        }
    }
    let outcome = if rejected { "failed" } else { "confirming" };
    finish(ctx, account, id, saved, outcome, campaign).await
}

fn receipt(mut campaign: Value, body: &Value) -> Value {
    for (field, value) in [
        ("claimedAt", campaign_time(body.get("claimedAt"))),
        ("expiresAt", campaign_time(body.get("expiresAt"))),
    ] {
        if let Some(value) = value { campaign[field] = json!(value); }
    }
    if let Some(amount) = body.pointer("/benefit/amount").and_then(Value::as_f64).filter(|amount| *amount >= 0.0) {
        campaign["amount"] = json!(amount);
    }
    campaign
}

async fn finish(
    ctx: &PluginCtx,
    account: &str,
    id: &str,
    mut saved: Value,
    outcome: &str,
    mut campaign: Value,
) -> Result<Value, GatewayError> {
    let state = match outcome {
        "claimed" | "already_claimed" => "claimed",
        "confirming" => "confirming",
        "inactive" => "inactive",
        _ => "ineligible",
    };
    campaign["status"] = json!(state);
    if matches!(state, "claimed" | "confirming") {
        saved["claims"][id] = json!({
            "at": now_ms(), "status": state,
            "startsAt": campaign.get("startsAt"), "endsAt": campaign.get("endsAt"),
            "claimedAt": campaign.get("claimedAt"), "expiresAt": campaign.get("expiresAt"),
        });
    } else if let Some(claims) = saved.get_mut("claims").and_then(Value::as_object_mut) {
        claims.remove(id);
    }
    if ctx.ledger_put(account, LEDGER_KIND, &saved).await.is_err() {
        crate::logging::verbose("[Qoder]", "活动领取记录保存失败，下次查询以上游状态为准");
    }
    Ok(result(id, outcome, Some(campaign)))
}

fn result(id: &str, outcome: &str, campaign: Option<Value>) -> Value {
    json!({ "campaignId": id, "outcome": outcome, "campaign": campaign })
}
