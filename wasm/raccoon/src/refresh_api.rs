//! 旧refresh接口可迁移成IAM；一旦迁移，丢弃所有旧刷新令牌并取平台user_info.id。

use super::{credentials::Credential, endpoints, errors, iam, jwt, network, refresh_state, util};
use agent2api_wasm_sdk::{json, Error, Result};

pub fn call(previous: &Credential) -> Result<Credential> {
    if !previous.can_refresh() {
        return Err(Error::http(401, "小浣熊该登录态不能续期，请重新登录"));
    }
    refresh_state::reserve(previous)?;
    let reply = network::raw(
        "POST",
        &format!("{}/refresh", endpoints::AUTH),
        &previous.token,
        "",
        json!({ "Accept": "application/json" }),
        Some(&json!({ "refresh_token": previous.refresh })),
    )?;
    if errors::failed(reply.status, &reply.value) {
        return Err(errors::upstream(
            reply.status,
            &reply.value,
            &[&previous.token, &previous.refresh],
        ));
    }
    let data = network::data(reply, &[&previous.token, &previous.refresh], false)?;
    let token = jwt::strip(&util::text(&data, &["access_token"]));
    if token.is_empty() || jwt::claims(&token).is_none() {
        return Err(Error::http(502, "小浣熊续期没有返回有效access_token"));
    }
    let profile = if jwt::iam(&token) {
        Some(iam::profile(&token)?)
    } else {
        None
    };
    let mut next = previous.clone();
    next.user = profile
        .as_ref()
        .map(|profile| util::text(profile, &["id"]))
        .or_else(|| {
            jwt::claims(&token)
                .map(|claims| jwt::uid(&claims))
                .filter(|user| !user.is_empty())
        })
        .unwrap_or_else(|| previous.user.clone());
    if !previous.user.is_empty() && previous.user != "unknown" && next.user != previous.user {
        return Err(Error::http(
            409,
            "小浣熊续期返回了不同平台账号，未覆盖现有凭据",
        ));
    }
    next.refresh = if profile.is_some() {
        String::new()
    } else {
        let refresh = jwt::strip(&util::text(&data, &["refresh_token"]));
        if refresh == token {
            String::new()
        } else if refresh.is_empty() {
            previous.refresh.clone()
        } else {
            refresh
        }
    };
    next.expires = jwt::expiry(&token).or(previous.expires);
    next.token = token;
    Ok(next)
}
