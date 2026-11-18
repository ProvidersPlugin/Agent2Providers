//! 千问办公国际资料沿用原生 account-context/data.user 形状；国际字段兼容性属基线推断，未实测。
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
    let is_biz = view["isBiz"].as_bool().or_else(||view["is_biz"].as_bool())
        .or_else(||view["org_type"].as_str().map(|kind|kind.eq_ignore_ascii_case("enterprise")));
    if credentials.is_biz.is_some() && is_biz.is_some() && credentials.is_biz != is_biz {
        return Err(Error::http(409,"千问办公企业身份与原凭据不一致").with_code("identity_changed"));
    }
    credentials.is_biz = is_biz.or(credentials.is_biz);
    for (current, keys) in [(&mut credentials.organization, &["organizationId","organization_id","orgId","org_id"][..]),
        (&mut credentials.team,&["teamId","team_id"][..])] {
        let incoming=util::text(view,keys);
        if !current.is_empty()&&!incoming.is_empty()&&*current!=incoming { return Err(Error::http(409,"千问办公组织额度池已变化").with_code("identity_changed")); }
        if current.is_empty(){*current=incoming;}
    }
    let user = util::text(view, &["id", "userId", "user_id", "uid"]);
    if user.is_empty() {
        return Err(Error::http(502, "千问办公资料缺少用户身份，未添加空账号"));
    }
    util::identity(&user)?;
    if !credentials.user.is_empty() && credentials.user != user {
        return Err(Error::http(409, "千问办公资料与凭据身份不一致").with_code("identity_changed"));
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
