//! Qoder 活动的只读清单与领取请求。每日签到和手动福利共用鉴权，互不代领。

use agent2api_provider_protocol::WireProxy;
use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use crate::errors::GatewayError;
use crate::http::ApiResponse;

use super::credentials::Credentials;
use super::endpoints::{self, Site};
use super::{auth, refresh, risk};

pub(super) struct CampaignSession {
    pub credentials: Credentials,
    pub proxy: Option<WireProxy>,
    pub headers: Vec<(String, String)>,
    pub list: Value,
    pub risk_note: Option<String>,
}

pub(super) async fn load(
    ctx: &PluginCtx,
    site: Site,
    account: &str,
) -> Result<CampaignSession, GatewayError> {
    let credentials = refresh::ensure_fresh(ctx, site, account, false).await?;
    match load_with(ctx, site, account, credentials).await {
        Err(error) if error.status_code == 401 => {
            let credentials = refresh::ensure_fresh(ctx, site, account, true).await?;
            load_with(ctx, site, account, credentials).await
        }
        result => result,
    }
}

async fn load_with(
    ctx: &PluginCtx,
    site: Site,
    account: &str,
    credentials: Credentials,
) -> Result<CampaignSession, GatewayError> {
    if !site.supports_checkin() {
        return Err(GatewayError::with_status(400, "该提供商没有活动领取接口"));
    }
    let (record, _) = refresh::snapshot(ctx, site, account).await?;
    let proxy = auth::account_proxy(&record)?;
    let mut headers = endpoints::sash_headers(&credentials.access_token);
    let mut list = fetch(&credentials, proxy.as_ref(), &headers).await?;
    let mut risk_note = None;
    if site == Site::QoderIntl {
        let uid = list.get("uid").and_then(Value::as_str).unwrap_or("").trim();
        let (augmented, note) = risk::augment_headers(headers, site, uid).await;
        headers = augmented;
        risk_note = note;
        list = fetch(&credentials, proxy.as_ref(), &headers).await?;
    }
    Ok(CampaignSession { credentials, proxy, headers, list, risk_note })
}

pub(super) async fn fetch(
    credentials: &Credentials,
    proxy: Option<&WireProxy>,
    headers: &[(String, String)],
) -> Result<Value, GatewayError> {
    let url = format!("{}{}{}", credentials.site.open_api(), endpoints::CAMPAIGNS_PATH, endpoints::CAMPAIGNS_QUERY);
    let response = auth::request("GET", &url, None, headers, proxy).await?;
    let value = unwrap_data(auth::payload(response, "活动查询")?);
    if let Some(rows) = value.get("campaigns").and_then(Value::as_array) {
        let mut ids = std::collections::HashSet::new();
        if rows.iter().all(|row| row.is_object()
            && row.get("campaignId").and_then(Value::as_str)
                .is_some_and(|id| !id.trim().is_empty() && ids.insert(id))) {
            return Ok(value);
        }
    }
    Err(GatewayError::with_status(502, "活动清单格式异常，未确认领取状态"))
}

pub(super) async fn submit(
    session: &CampaignSession,
    campaign_id: &str,
) -> Result<ApiResponse, GatewayError> {
    let mut url = url::Url::parse(session.credentials.site.open_api())
        .map_err(|_| GatewayError::with_status(500, "活动接口地址无效"))?;
    url.set_path(endpoints::CAMPAIGNS_PATH);
    url.path_segments_mut()
        .map_err(|_| GatewayError::with_status(500, "活动接口地址无效"))?
        .push(campaign_id).push("claim");
    let mut headers = session.headers.clone();
    headers.push(("content-type".to_string(), "application/json".to_string()));
    headers.push(("origin".to_string(), session.credentials.site.open_api().to_string()));
    auth::request("POST", url.as_str(), Some(&json!({})), &headers, session.proxy.as_ref()).await
}

pub(super) fn unwrap_data(value: Value) -> Value {
    match value.get("data") {
        Some(data) if data.is_object() => data.clone(),
        Some(data) if data.is_array() => json!({ "campaigns": data }),
        _ => value,
    }
}

pub(super) fn campaign_time(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Some(number) = value.as_i64().or_else(|| value.as_str()?.parse().ok()) {
        return (number > 0).then(|| if number < 100_000_000_000 { number.saturating_mul(1000) } else { number });
    }
    chrono::DateTime::parse_from_rfc3339(value.as_str()?).ok().map(|time| time.timestamp_millis())
}

pub(super) fn campaign_text(row: &Value, locale: &str, field: &str) -> String {
    let placements = row.get("placements").and_then(Value::as_array);
    let text = placements.and_then(|placements| {
        ["POPUP", "USAGE"].into_iter().find_map(|kind| {
            placements.iter()
                .filter(|placement| placement.get("type").and_then(Value::as_str) == Some(kind))
                .find_map(|placement| placement.get("content")?.get(locale)?.get(field)?.as_str()
                    .filter(|text| !text.trim().is_empty()))
        })
    });
    text.or_else(|| row.get(field).and_then(Value::as_str)).unwrap_or("").trim().to_string()
}

pub(super) fn is_credit_benefit(row: &Value) -> bool {
    row.get("actionType").and_then(Value::as_str) == Some("CLAIM_BENEFIT")
        && row.pointer("/benefit/kind").and_then(Value::as_str) == Some("CREDITS")
        && row.get("campaignId").and_then(Value::as_str).is_some_and(|id| !id.trim().is_empty())
}

pub(super) fn is_daily(row: &Value) -> bool {
    if !is_credit_benefit(row) { return false; }
    let title = [campaign_text(row, "zh", "title"), campaign_text(row, "en", "title")].join(" ").to_lowercase();
    let scope = row.pointer("/benefit/modelScope");
    let all_models = scope.and_then(|scope| scope.pointer("/modelSeries/key")).and_then(Value::as_str) == Some("ALL_MODELS");
    let scoped = !all_models && scope.is_some_and(|scope| !scope.is_null() && scope.as_object().is_none_or(|object| !object.is_empty()));
    if scoped || title.contains("sonus") || title.contains("sota") { return false; }
    if title.contains("每日") || title.contains("每天") || title.contains("daily") || title.contains("every day") { return true; }
    // 旧活动未附标题时，仅认已确认的「100 Credits、30 天有效、单日窗口」。
    let one_day = match (campaign_time(row.get("startAt")), campaign_time(row.get("endAt"))) {
        (Some(start), Some(end)) => end > start && end - start <= 86_400_000,
        _ => false,
    };
    one_day && row.pointer("/benefit/amount").and_then(Value::as_f64) == Some(100.0)
        && row.pointer("/benefit/validity/days").and_then(Value::as_f64) == Some(30.0)
}
