//! 千问办公国际设备凭据，只保留 refresh|userId|machineId；无 PAT 或作业令牌兑换。
//! provider 固定；global/intl 仅兼容标签，缺标签不成为查找或覆盖国内账号的依据。
//! 已有 machine_id 优先保留并核对别名，凭据结构及国际续期语义沿用原生基线，尚未实测。
use super::{endpoints, machine, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

#[derive(Clone)]
pub struct Credentials {
    pub access: String,
    pub refresh: String,
    pub expires: Option<i64>,
    pub user: String,
    pub machine: String,
    pub name: String,
    pub email: String,
    pub is_biz: Option<bool>,
    pub organization: String,
    pub team: String,
    pub realm: String,
}
pub fn decode(value: &Value) -> Result<Value> {
    fn object(value: &Value) -> Result<Value> {
        let value = if let Some(raw) = value.as_str() {
            if raw.len() > 256 * 1024 {
                return Err(Error::http(400, "千问办公凭据 JSON 过大"));
            }
            serde_json::from_str(raw).map_err(|_| Error::http(400, "千问办公凭据不是有效 JSON"))?
        } else {
            value.clone()
        };
        if !value.is_object() || value.to_string().len() > 256 * 1024 {
            return Err(Error::http(400, "千问办公凭据必须是有界 JSON 对象"));
        }
        owner(&value)?;
        if value["desktop"] == true || value["importDesktop"] == true {
            return Err(Error::http(
                400,
                "千问办公国际版不读取本机凭据，请使用设备授权或凭据文本",
            ));
        }
        Ok(value)
    }
    let mut value = object(value)?;
    for _ in 0..4 {
        let child = ["secret", "auth", "data"].iter().find_map(|key| {
            value
                .get(*key)
                .filter(|child| child.is_object() || child.is_string())
                .cloned()
        });
        let Some(child) = child else { break };
        let mut child = object(&child)?;
        let outer_user = user_id(&value)?;
        let inner_user = user_id(&child)?;
        let outer_machine =
            identity_fields(&value, &["machine_id", "machineId", "loginDeviceId"], false)?;
        let inner_machine =
            identity_fields(&child, &["machine_id", "machineId", "loginDeviceId"], false)?;
        if !outer_user.is_empty() && !inner_user.is_empty() && outer_user != inner_user
            || !outer_machine.is_empty()
                && !inner_machine.is_empty()
                && outer_machine != inner_machine
        {
            return Err(
                Error::http(409, "千问办公外层与嵌套凭据身份不一致").with_code("identity_changed")
            );
        }
        for (key, outer) in value.as_object().into_iter().flatten() {
            if ["secret", "auth", "data"].contains(&key.as_str()) {
                continue;
            }
            let group = [
                &[
                    "accessToken",
                    "access",
                    "token",
                    "access_token",
                    "device_token",
                    "deviceToken",
                ][..],
                &["refreshToken", "refresh", "refresh_token"][..],
                &["expiresAt", "expires", "expires_at", "tokenExpiresAt"][..],
                &["machine_id", "machineId", "loginDeviceId"][..],
                &["userId", "user_id", "uid"][..],
            ]
            .into_iter()
            .find(|group| group.contains(&key.as_str()));
            if group.is_some_and(|keys| {
                keys.iter()
                    .any(|key| child.get(*key).is_some_and(|v| !v.is_null() && v != ""))
            }) {
                continue;
            }
            if child.get(key).is_none_or(|v| v.is_null() || v == "") {
                child[key] = outer.clone();
            }
        }
        value = child;
    }
    if value["desktop"] == true || value["importDesktop"] == true {
        return Err(Error::http(
            400,
            "千问办公国际版不读取本机凭据，请使用设备授权或凭据文本",
        ));
    }
    if ["pat", "personalAccessToken", "personal_token"]
        .iter()
        .any(|key| value.get(*key).is_some_and(|v| !v.is_null() && v != ""))
    {
        return Err(Error::http(
            400,
            "千问办公不使用个人访问令牌，请使用本家的设备授权或设备凭据",
        ));
    }
    Ok(value)
}
pub fn owner(value: &Value) -> Result<()> {
    for key in ["provider", "mode", "edition", "region"] {
        let Some(raw) = value.get(key).filter(|value| !value.is_null()) else {
            continue;
        };
        let raw = raw
            .as_str()
            .ok_or_else(|| Error::http(400, "千问办公身份标签格式无效"))?
            .trim();
        let compatible = key != "provider"
            && matches!(raw.to_ascii_lowercase().as_str(), "global" | "intl")
            || key == "edition"
                && raw.eq_ignore_ascii_case("qwenwork")
                && (value["schemaVersion"] == 2 || value["auth"]["schemaVersion"] == 2);
        if !raw.is_empty() && raw != endpoints::PROVIDER && !compatible {
            return Err(Error::http(400, "当前插件只接受千问办公国际版凭据"));
        }
    }
    // 固定国际公有云来源，不能把企业自定义域、国内版或其它产品记录默默转投本家。
    for key in [
        "baseUrl", "base_url", "endpoint", "gateway", "openApi", "origin",
    ] {
        let Some(raw) = value
            .get(key)
            .filter(|value| !value.is_null() && *value != "")
        else {
            continue;
        };
        if raw
            .as_str()
            .is_none_or(|raw| raw.trim().trim_end_matches('/') != endpoints::ORIGIN)
        {
            return Err(Error::coded(
                "千问办公国际版不接受其他服务地址",
                "forbidden",
            ));
        }
    }
    for (key, expected) in [
        ("oauthClientId", endpoints::CLIENT_ID),
        ("oauthRedirectUri", endpoints::REDIRECT_URI),
    ] {
        if value
            .get(key)
            .is_some_and(|raw| !raw.is_null() && raw != "" && raw.as_str() != Some(expected))
        {
            return Err(Error::coded(
                "千问办公凭据授权参数不属于国际版",
                "forbidden",
            ));
        }
    }
    Ok(())
}
fn identity_fields(value: &Value, keys: &[&str], numeric: bool) -> Result<String> {
    let mut found = String::new();
    for key in keys {
        let Some(raw) = value.get(*key).filter(|raw| !raw.is_null() && *raw != "") else {
            continue;
        };
        let text = match raw {
            Value::String(text) => text.trim().to_string(),
            Value::Number(number) if numeric => number.to_string(),
            _ => return Err(Error::http(400, "千问办公用户或机器编号必须使用有效文本")),
        };
        util::identity(&text)?;
        if text.is_empty() {
            continue;
        }
        if !found.is_empty() && found != text {
            return Err(
                Error::http(409, "千问办公凭据身份字段不一致").with_code("identity_changed")
            );
        }
        found = text;
    }
    Ok(found)
}
pub fn user_id(value: &Value) -> Result<String> {
    let outer = identity_fields(value, &["userId", "user_id", "uid"], true)?;
    let inner = value
        .get("user")
        .filter(|v| v.is_object())
        .map(|user| identity_fields(user, &["id", "userId", "user_id", "uid"], true))
        .transpose()?
        .unwrap_or_default();
    if !outer.is_empty() && !inner.is_empty() && outer != inner {
        return Err(
            Error::http(409, "千问办公用户资料与顶层身份不一致").with_code("identity_changed")
        );
    }
    Ok(if outer.is_empty() { inner } else { outer })
}
impl Credentials {
    pub fn parse(value: &Value) -> Result<Self> {
        let value = decode(value)?;
        let access = util::secret(
            &value,
            &[
                "accessToken",
                "access",
                "token",
                "access_token",
                "device_token",
                "deviceToken",
            ],
        )?;
        let refresh = util::secret(&value, &["refreshToken", "refresh", "refresh_token"])?;
        if access.is_empty() || access.contains('|') {
            return Err(Error::http(400, "千问办公缺少有效访问凭据"));
        }
        if access.starts_with("jt-") || refresh.starts_with("jrt-") || refresh.starts_with("pat|") {
            return Err(Error::http(
                400,
                "千问办公只接收设备凭据，不接收其他产品的作业令牌或 PAT",
            ));
        }
        let parts = refresh.split('|').collect::<Vec<_>>();
        let (packed_user, packed_machine) = match parts.as_slice() {
            [_, user, machine] => (*user, *machine),
            [_] => ("", ""),
            _ => return Err(Error::http(400, "千问办公刷新凭据格式无效，请粘贴完整记录")),
        };
        let mut user = user_id(&value)?;
        let mut machine =
            identity_fields(&value, &["machine_id", "machineId", "loginDeviceId"], false)?;
        if !user.is_empty() && !packed_user.is_empty() && user != packed_user
            || !machine.is_empty() && !packed_machine.is_empty() && machine != packed_machine
        {
            return Err(Error::http(409, "千问办公凭据中的用户或设备编号不一致")
                .with_code("identity_changed"));
        }
        if user.is_empty() {
            user = packed_user.into();
        }
        if machine.is_empty() {
            machine = packed_machine.into();
        }
        let mut lifetime = value.clone();
        if let Some(fields) = lifetime.as_object_mut() {
            fields.remove("expires_in");
            fields.remove("expiresIn");
        }
        let expires = util::token_expiry(&lifetime, &access)?;
        let mut result = Self {
            is_biz: value["isBiz"].as_bool().or_else(||value["is_biz"].as_bool()).or_else(||value["user"]["isBiz"].as_bool()).or_else(||value["user"]["is_biz"].as_bool()),
            organization: util::text(&value,&["organizationId","organization_id","orgId","org_id"]),
            team: util::text(&value,&["teamId","team_id"]),
            realm: util::text(&value,&["authRealm"]),
            access,
            refresh: refresh.clone(),
            expires,
            user,
            machine,
            name: {
                let top = util::text(&value, &["nickname", "name", "username"]);
                if top.is_empty() {
                    util::text(&value["user"], &["name", "username", "user_name"])
                } else {
                    top
                }
            },
            email: {
                let top = util::text(&value, &["email"]);
                if top.is_empty() {
                    util::text(&value["user"], &["email"])
                } else {
                    top
                }
            },
        };
        if parts.len() == 1 && !refresh.is_empty() {
            result.refresh = format!("{refresh}|{}|{}", result.user, result.machine);
        }
        result.validate_public()?;
        Ok(result)
    }
    pub fn complete(&mut self) -> Result<()> {
        if self.user.is_empty() {
            return Err(Error::http(
                400,
                "千问办公缺少用户身份，请重新授权或导入完整记录",
            ));
        }
        util::identity(&self.user)?;
        self.machine = machine::choose(&self.machine)?;
        if !self.refresh.is_empty() {
            self.refresh = format!("{}|{}|{}", self.oauth_refresh(), self.user, self.machine);
        }
        if self.refresh.len() > endpoints::MAX_TOKEN {
            return Err(Error::http(400, "千问办公刷新凭据过长"));
        }
        self.validate_public()
    }
    pub fn oauth_refresh(&self) -> &str {
        self.refresh.split('|').next().unwrap_or("")
    }
    pub fn can_refresh(&self) -> bool {
        !self.oauth_refresh().is_empty()
    }
    pub fn expiring(&self) -> Result<bool> {
        let now = util::now()?;
        Ok(self.is_biz != Some(true) && self.can_refresh()
            && self
                .expires
                .is_some_and(|expires| expires <= now.saturating_add(endpoints::REFRESH_MARGIN)))
    }
    pub fn validate_public(&self) -> Result<()> {
        util::identity(&self.user)?;
        util::identity(&self.machine)?;
        util::identity(&self.organization)?;
        util::identity(&self.team)?;
        util::identity(&self.realm)?;
        if self.name.chars().count() > 100
            || self.email.chars().count() > 320
            || [&self.name, &self.email]
                .iter()
                .any(|value| value.chars().any(char::is_control))
        {
            return Err(Error::http(400, "千问办公公开账号信息格式无效"));
        }
        let secrets = [&self.access[..], &self.refresh[..], self.oauth_refresh()];
        if [&self.name, &self.email, &self.user, &self.machine, &self.organization, &self.team, &self.realm]
            .iter()
            .any(|public| {
                secrets
                    .iter()
                    .any(|secret| !secret.is_empty() && public.contains(*secret))
            })
        {
            return Err(Error::http(400, "千问办公公开字段不能包含完整凭据"));
        }
        Ok(())
    }
    pub fn value(&self) -> Value {
        let nullable = |value: &str| {
            if value.is_empty() {
                Value::Null
            } else {
                json!(value)
            }
        };
        json!({ "provider": endpoints::PROVIDER, "mode": "global", "accessToken": self.access,
            "refreshToken": nullable(&self.refresh), "expiresAt": self.expires, "userId": self.user,
            "machineId": self.machine, "nickname": nullable(&self.name), "email": nullable(&self.email),
            "isBiz":self.is_biz,"organizationId":nullable(&self.organization),"teamId":nullable(&self.team),"authRealm":nullable(&self.realm) })
    }
    pub fn binding(&self) -> String {
        util::hash(&json!([endpoints::PROVIDER, self.user, self.machine, self.is_biz, self.organization, self.team, self.realm]).to_string())
    }
}
