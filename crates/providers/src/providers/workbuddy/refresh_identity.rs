//! 新 token 必须能取回原账号身份，不能只因刷新接口返回成功就换掉本地凭证。
//!
//! ── 空字段 = 上游没给，不是「身份变了」──────────────────────
//! 资料接口会随上游版本增删字段：2026-11 起国内版 `/v2/plugin/account` 对部分
//! 账号把 `type` 从 `"personal"` 改成空串，`enterpriseId` 也不再下发；海外版
//! 的 `enterpriseId` 同样可能是 `null`。这些空值只说明「这一栏没填」，不能据此
//! 认定换人了 —— 判据因此收紧到：`uid` 必须一致；企业 / 账号类型只在**两边都
//! 有值**时才必须一致。落库时空值也不覆盖原有的身份字段。

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
    let same_uid: Vec<&Value> = rows.iter().filter(|row| text(row, "uid") == uid).collect();
    if same_uid.is_empty() {
        return Err(mismatch("uid"));
    }
    let old_ent = text(previous, "enterpriseId");
    let old_type = text(previous, "type");
    let mut matching = same_uid;
    // 企业：存量有值就优先找同名企业；上游没给（空 / null）算「未知」，不否决。
    // 存量是个人号（空）时优先挑同 uid 的个人行，避免同名个人 / 企业行混淆。
    if old_ent.is_empty() {
        let personal: Vec<&Value> = matching
            .iter()
            .copied()
            .filter(|row| text(row, "enterpriseId").is_empty())
            .collect();
        if !personal.is_empty() {
            matching = personal;
        }
    } else {
        let exact: Vec<&Value> = matching
            .iter()
            .copied()
            .filter(|row| text(row, "enterpriseId") == old_ent)
            .collect();
        if !exact.is_empty() {
            matching = exact;
        } else {
            matching.retain(|row| text(row, "enterpriseId").is_empty());
            if matching.is_empty() {
                return Err(mismatch("企业"));
            }
        }
    }
    // 账号类型同理：上游给空串是「没填」，只有明确给了另一个值才算变了。
    if !old_type.is_empty() {
        let exact: Vec<&Value> = matching
            .iter()
            .copied()
            .filter(|row| text(row, "type") == old_type)
            .collect();
        if !exact.is_empty() {
            matching = exact;
        } else {
            matching.retain(|row| text(row, "type").is_empty());
            if matching.is_empty() {
                return Err(mismatch("账号类型"));
            }
        }
    }
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
        if let Some(value) = hit.get(key).filter(|value| present(value)) {
            merged.insert(key.into(), normalized(value));
        }
    }
    for (key, value) in [("domain", &next.domain), ("tokenType", &next.token_type)] {
        if let Some(value) = value.as_ref().filter(|value| present(value)) {
            merged.insert(key.into(), normalized(value));
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

/// 取字段里的非空字符串（去首尾空白）；缺失 / `null` / 空串一律返回空串。
fn text<'a>(row: &'a Value, key: &str) -> &'a str {
    row.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
}

/// 上游「确实给了这一栏」的判据：`null` / 空串 / 全空白都算没给。
fn present(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.trim().is_empty(),
        _ => true,
    }
}

/// 字符串字段去掉首尾空白后再落库，避免 `" personal "` 与原值逐字比较时假冲突。
fn normalized(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(text.trim().to_string()),
        other => other.clone(),
    }
}

fn mismatch(field: &str) -> GatewayError {
    GatewayError::with_status(
        409,
        format!("WorkBuddy 续期后的{field}与原身份不一致或无法确认，请重新登录；未替换本地身份"),
    )
    .with_code("refresh_identity_mismatch")
}
