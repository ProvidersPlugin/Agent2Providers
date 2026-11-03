//! 新 token 必须能取回原账号身份，不能只因刷新接口返回成功就换掉本地凭证。

use agent2api_provider_protocol::WireProxy;
use serde_json::{json, Value};

use super::{auth::RefreshedAuth, contract, region::Region};
use crate::errors::GatewayError;

pub(super) async fn verify(
    previous: &Value,
    next: &RefreshedAuth,
    session: &Value,
    region: Region,
    proxy: Option<&WireProxy>,
) -> Result<Value, GatewayError> {
    let old_token = previous
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or("");
    if next.access_token.is_empty() || next.access_token == old_token {
        return Err(GatewayError::with_status(
            502,
            "WorkBuddy 续期没有产生新的访问凭证",
        ));
    }
    let uid = previous
        .get("uid")
        .and_then(Value::as_str)
        .filter(|uid| !uid.is_empty())
        .ok_or_else(|| mismatch("uid"))?;
    if let Some(claims) = crate::crypto::decode_jwt_claims(&next.access_token) {
        if let Some(sub) = claims
            .get("sub")
            .and_then(Value::as_str)
            .filter(|sub| !sub.is_empty())
        {
            if sub != uid {
                return Err(mismatch("uid"));
            }
        }
    }
    let mut fresh = session.clone();
    fresh["auth"]["accessToken"] = json!(next.access_token);
    if let Some(domain) = &next.domain {
        fresh["auth"]["domain"] = domain.clone();
    }
    if let Some(token_type) = &next.token_type {
        fresh["auth"]["tokenType"] = token_type.clone();
    }
    let path = match region {
        Region::Cn => "/v2/plugin/account",
        Region::Intl => "/v2/plugin/accounts",
    };
    let url = format!("{}{path}", super::session_endpoint_of(session, region));
    let headers = contract::framework_headers(&fresh, region, false);
    let response = crate::http::send_raw("GET", &url, None, &headers, proxy, Some(4_000))
        .await
        .map_err(|_| GatewayError::with_status(502, "续期后无法确认账号身份，未替换本地登录态"))?;
    let payload = response
        .payload
        .as_ref()
        .ok_or_else(|| mismatch("账号资料响应"))?;
    if !response.ok
        || payload.get("code").is_some_and(|code| {
            code.as_i64()
                .or_else(|| code.as_str().and_then(|text| text.parse().ok()))
                .is_some_and(|code| code != 0)
        })
    {
        return Err(GatewayError::with_status(
            i32::from(response.status).max(400),
            "续期后账号资料被上游拒绝，未确认登录态",
        ));
    }
    let data = payload.get("data").ok_or_else(|| mismatch("账号资料"))?;
    let rows = if region == Region::Cn {
        vec![data.clone()]
    } else {
        data.get("accounts")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| mismatch("账号列表"))?
    };
    let same_uid: Vec<&Value> = rows
        .iter()
        .filter(|row| row.get("uid").and_then(Value::as_str) == Some(uid))
        .collect();
    let old_ent = previous
        .get("enterpriseId")
        .and_then(Value::as_str)
        .unwrap_or("");
    let old_type = previous
        .get("type")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let matching: Vec<&Value> = same_uid
        .into_iter()
        .filter(|row| {
            row.get("enterpriseId")
                .and_then(Value::as_str)
                .unwrap_or("")
                == old_ent
                && old_type.is_none_or(|old_type| {
                    row.get("type").and_then(Value::as_str) == Some(old_type)
                })
        })
        .collect();
    if matching.len() != 1 {
        return Err(mismatch("uid / 企业 / 账号类型"));
    }
    let hit = matching[0];
    let mut merged = previous.as_object().cloned().unwrap_or_default();
    for key in [
        "type",
        "oneidAccountId",
        "departmentFullName",
        "idp",
        "nickname",
        "enterpriseName",
    ] {
        if let Some(value) = hit.get(key) {
            merged.insert(key.into(), value.clone());
        }
    }
    if previous
        .get("idp")
        .and_then(Value::as_str)
        .is_some_and(|idp| !idp.is_empty())
        && merged
            .get("idp")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        if let Some(value) = previous.get("idp") {
            merged.insert("idp".into(), value.clone());
        }
    }
    for (key, value) in [("domain", &next.domain), ("tokenType", &next.token_type)] {
        if let Some(value) = value {
            merged.insert(key.into(), value.clone());
        }
    }
    for key in [
        "type",
        "oneidAccountId",
        "departmentFullName",
        "idp",
        "domain",
        "tokenType",
    ] {
        if let Some(previous) = previous.get(key).filter(|value| {
            !value.is_null() && !value.as_str().is_some_and(|value| value.trim().is_empty())
        }) {
            if merged.get(key) != Some(previous) {
                return Err(mismatch(key));
            }
        }
    }
    Ok(Value::Object(merged))
}

fn mismatch(field: &str) -> GatewayError {
    GatewayError::with_status(
        409,
        format!("WorkBuddy 续期后的{field}与原身份不一致或无法确认，请重新登录；未替换本地身份"),
    )
    .with_code("refresh_identity_mismatch")
}
