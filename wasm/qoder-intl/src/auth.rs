//! PAT 兑换和资料查询使用固定国际 openapi 的普通 Bearer，不需要 UMID 或 COSY。
use super::{client, credentials::Credentials, endpoints, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn profile(credentials: &mut Credentials) -> Result<()> {
    let reply = client::get(endpoints::USER_INFO, &credentials.access, "用户资料查询")?;
    let value = client::accepted(reply, "用户资料查询")?;
    let user = util::text(&value, &["id", "userId", "user_id", "uid"]);
    if user.is_empty() {
        return Err(Error::http(502, "Qoder 用户资料缺少身份，未添加空账号"));
    }
    util::identity(&user)?;
    if !credentials.user.is_empty() && credentials.user != user {
        return Err(
            Error::http(409, "Qoder 用户资料与凭据身份不一致").with_code("identity_changed")
        );
    }
    credentials.user = user;
    let name = util::text(&value, &["name", "username", "user_name"]);
    let email = util::text(&value, &["email"]);
    if !name.is_empty() {
        credentials.name = name.chars().take(100).collect();
    }
    if !email.is_empty() {
        credentials.email = email.chars().take(320).collect();
    }
    credentials.validate_public()
}
pub fn exchange_pat(raw: &str, machine: Option<&str>) -> Result<Credentials> {
    let pat = util::secret(&json!({ "pat": raw }), &["pat"])?;
    if pat.is_empty() || pat.contains('|') {
        return Err(Error::http(400, "请填写有效 Qoder 国际版个人访问令牌"));
    }
    let reply = client::request(
        "POST",
        endpoints::OPEN_API,
        endpoints::EXCHANGE,
        client::account_headers(None),
        Some(&json!({ "personal_token": pat })),
        "个人访问令牌兑换",
    )?;
    let data = client::accepted(reply, "个人访问令牌兑换")?;
    let access = util::secret(
        &data,
        &["token", "device_token", "access_token", "accessToken"],
    )?;
    let refresh = util::secret(&data, &["refresh_token", "refreshToken"])?;
    if access.is_empty() || access.contains('|') || refresh.contains('|') {
        return Err(Error::http(502, "Qoder PAT 兑换未返回有效凭据"));
    }
    let machine = match machine.filter(|value| !value.is_empty()) {
        Some(value) => value.to_string(),
        None => util::uuid()?,
    };
    util::identity(&machine)?;
    let mut credentials = Credentials::parse(
        &json!({ "provider": endpoints::PROVIDER, "mode": "global", "accessToken": access,
        "machineId": machine, "expiresAt": util::expiry(&data, endpoints::PAT_LIFETIME)? }),
    )?;
    profile(&mut credentials)?;
    credentials.refresh = format!(
        "pat|{pat}|{refresh}|{}|{}",
        credentials.user, credentials.machine
    );
    credentials.complete()?;
    Ok(credentials)
}
pub fn prepare(value: &Value) -> Result<Credentials> {
    let pat = util::secret(value, &["pat", "personalAccessToken", "personal_token"])?;
    if !pat.is_empty() {
        let machine = util::text(value, &["machineId", "machine_id"]);
        util::identity(&machine)?;
        return exchange_pat(&pat, Some(&machine));
    }
    let mut credentials = Credentials::parse(value)?;
    if credentials.user.is_empty() {
        profile(&mut credentials)?;
    }
    credentials.complete()?;
    Ok(credentials)
}
