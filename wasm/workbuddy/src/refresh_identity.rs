//! 国内资料只读/v2/plugin/account；uid必须同一，空身份字段不覆盖旧值。
use super::{endpoints, headers, network, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

fn mismatch() -> Error {
    Error::http(
        409,
        "WorkBuddy续期后的账号、企业或授权身份与原记录不一致，未替换登录态",
    )
    .with_code("refresh_identity_mismatch")
}
pub fn verify(previous: &Value, auth: &Value) -> Result<Value> {
    let uid = util::text(previous, &["uid"]);
    let token = util::text(auth, &["accessToken"]);
    if uid.is_empty() || token.is_empty() {
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
        if let Some(value) = auth
            .get(key)
            .filter(|v| !v.is_null() && !v.as_str().is_some_and(|s| s.trim().is_empty()))
        {
            fresh[key] = value.clone();
        }
    }
    let profile = network::raw_timeout(
        "GET",
        &endpoints::endpoint(previous)?,
        "/v2/plugin/account",
        headers::login_account(&fresh)?,
        None,
        4000,
    )?
    .checked(&[
        &token,
        auth["refreshToken"].as_str().unwrap_or(""),
        previous["accessToken"].as_str().unwrap_or(""),
        previous["refreshToken"].as_str().unwrap_or(""),
    ])?;
    if util::text(&profile, &["uid"]) != uid {
        return Err(mismatch());
    }
    let previous_ent = util::text(previous, &["enterpriseId"]);
    let next_ent = util::text(&profile, &["enterpriseId"]);
    if !next_ent.is_empty() && next_ent != previous_ent {
        return Err(mismatch());
    }
    let mut result = previous.clone();
    for key in [
        "type",
        "oneidAccountId",
        "departmentFullName",
        "idp",
        "nickname",
        "enterpriseName",
    ] {
        let value = util::text(&profile, &[key]);
        if !value.is_empty() {
            result[key] = json!(value);
        }
    }
    for key in ["domain", "tokenType"] {
        let value = util::text(auth, &[key]);
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
        if !old.is_empty() && util::text(&result, &[key]) != old {
            return Err(mismatch());
        }
    }
    Ok(result)
}
