//! 千问办公国内账号资料来自 account-context 的 data.user，不能用 Qoder userinfo。
//! 资料缺失不能生成空账号，返回用户与已给身份冲突时停止，不公开任何凭据。
use super::{client, credentials::Credentials, endpoints, util};
use agent2api_wasm_sdk::{Error, Result, Value};

pub fn user(value: &Value) -> &Value {
    let data = client::unwrap_data(value);
    data.get("user")
        .filter(|value| value.is_object())
        .unwrap_or(data)
}
pub fn profile(credentials: &mut Credentials) -> Result<()> {
    let reply = client::get(endpoints::USER_INFO, &credentials.access, "用户资料查询")?;
    let value = client::accepted(reply, "用户资料查询")?;
    let view = user(&value);
    let user = util::text(view, &["id", "userId", "user_id", "uid"]);
    if user.is_empty() {
        return Err(Error::http(502, "千问办公资料缺少用户身份，未添加空账号"));
    }
    util::identity(&user)?;
    if !credentials.user.is_empty() && credentials.user != user {
        return Err(Error::http(409, "千问办公资料与凭据身份不一致").with_code("identity_changed"));
    }
    let is_biz = view
        .get("is_biz")
        .or_else(|| view.get("isBiz"))
        .filter(|value| !value.is_null())
        .and_then(Value::as_bool);
    let organization = util::text(view, &["orgId", "organization_id", "organizationId"]);
    let organization = if organization.is_empty() {
        util::text(&view["organization"], &["org_id", "orgId", "id"])
    } else {
        organization
    };
    let team = util::text(view, &["teamId", "team_id"]);
    let team = if team.is_empty() {
        util::text(&view["organization"], &["team_id", "teamId"])
    } else {
        team
    };
    if credentials.is_biz == Some(true) && (is_biz != Some(true) || team.is_empty()) {
        return Err(
            Error::http(409, "千问办公企业额度池身份未获上游确认").with_code("identity_changed")
        );
    }
    if credentials.is_biz.is_some() && is_biz.is_some() && credentials.is_biz != is_biz
        || !credentials.organization.is_empty()
            && !organization.is_empty()
            && credentials.organization != organization
        || !credentials.team.is_empty() && !team.is_empty() && credentials.team != team
    {
        return Err(
            Error::http(409, "千问办公资料与原账号额度池身份不一致").with_code("identity_changed")
        );
    }
    if let Some(is_biz) = is_biz {
        credentials.is_biz = Some(is_biz);
    }
    if !organization.is_empty() {
        credentials.organization = organization;
    }
    if !team.is_empty() {
        credentials.team = team;
    }
    credentials.user = user;
    let name = util::text(view, &["name", "username", "user_name"]);
    let email = util::text(view, &["email"]);
    if !name.is_empty() {
        credentials.name = name.chars().take(100).collect();
    }
    if !email.is_empty() {
        credentials.email = email.chars().take(320).collect();
    }
    credentials.validate_public()
}
pub fn prepare(value: &Value) -> Result<Credentials> {
    let mut credentials = Credentials::parse(value)?;
    if credentials.user.is_empty() || credentials.is_biz == Some(true) {
        profile(&mut credentials)?;
    }
    if credentials.expires.is_none() {
        credentials.expires = util::token_expiry(value, &credentials.access)?;
    }
    credentials.complete()?;
    Ok(credentials)
}
