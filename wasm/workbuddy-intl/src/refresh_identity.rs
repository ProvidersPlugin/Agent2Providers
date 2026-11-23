//! 国际续期必须从/v2/plugin/accounts唯一选回原uid、企业与授权身份；不是国内/account。
use crate::{account, endpoints, headers, network, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

fn mismatch() -> Error {
    Error::http(
        409,
        "国际版续期后的账号、企业或授权身份不一致或无法唯一确认，未替换登录态",
    )
    .with_code("refresh_identity_mismatch")
}
pub fn verify(previous: &Value, auth: &Value) -> Result<Value> {
    let uid = util::text(previous, &["uid"]);
    let token = util::text(auth, &["accessToken"]);
    if uid.is_empty() || token.is_empty() || previous["accessToken"] == token {
        return Err(mismatch());
    }
    if util::claims(&token)
        .and_then(|v| v["sub"].as_str().map(str::to_string))
        .is_some_and(|sub| !sub.is_empty() && sub != uid)
    {
        return Err(mismatch());
    }
    let mut fresh = previous.clone();
    fresh["accessToken"] = json!(token);
    for key in ["domain", "tokenType"] {
        let value = util::text(auth, &[key]);
        let value = if key == "domain" {
            account::canonical_domain(&value).to_string()
        } else {
            value
        };
        if !value.is_empty() {
            fresh[key] = json!(value);
        }
    }
    let data = network::raw_timeout(
        "GET",
        &endpoints::endpoint(previous)?,
        "/v2/plugin/accounts",
        headers::refresh(&fresh)?,
        None,
        4000,
    )?
    .checked(&[
        &token,
        auth["refreshToken"].as_str().unwrap_or(""),
        previous["accessToken"].as_str().unwrap_or(""),
        previous["refreshToken"].as_str().unwrap_or(""),
    ])?;
    let rows = data["accounts"].as_array().ok_or_else(mismatch)?;
    let mut matching: Vec<_> = rows
        .iter()
        .filter(|row| util::text(row, &["uid"]) == uid)
        .collect();
    for key in ["enterpriseId", "type"] {
        let old = util::text(previous, &[key]);
        if old.is_empty() {
            if key == "enterpriseId" {
                // 个人号不能悄悄换成同uid的企业账号。
                matching.retain(|row| util::text(row, &[key]).is_empty());
            }
            continue;
        }
        let exact: Vec<_> = matching
            .iter()
            .copied()
            .filter(|row| util::text(row, &[key]) == old)
            .collect();
        if !exact.is_empty() {
            matching = exact;
        } else {
            matching.retain(|row| util::text(row, &[key]).is_empty());
        }
    }
    if matching.len() != 1 {
        return Err(mismatch());
    }
    let hit = matching[0];
    let mut result = previous.clone();
    for key in [
        "type",
        "oneidAccountId",
        "departmentFullName",
        "idp",
        "nickname",
        "enterpriseName",
    ] {
        let value = util::text(hit, &[key]);
        if !value.is_empty() {
            result[key] = json!(value);
        }
    }
    for key in ["domain", "tokenType"] {
        let value = util::text(auth, &[key]);
        let value = if key == "domain" {
            account::canonical_domain(&value).to_string()
        } else {
            value
        };
        if !value.is_empty() {
            result[key] = json!(value);
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
        let old = util::text(previous, &[key]);
        let new = util::text(&result, &[key]);
        let matches = if key == "domain" {
            account::canonical_domain(&old) == account::canonical_domain(&new)
        } else {
            old == new
        };
        if !old.is_empty() && !matches {
            return Err(mismatch());
        }
    }
    Ok(result)
}
