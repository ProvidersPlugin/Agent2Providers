//! 千问办公国内设备凭据，只保留 refresh|userId|machineId；无 PAT 或作业令牌兑换。
//! provider 固定，cn 仅兼容标签。已有 machine_id 优先复用并核对其他别名，不用地区字段切换身份。
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
}
pub fn decode(value: &Value) -> Result<Value> {
    let mut value = if let Some(raw) = value.as_str() {
        if raw.len() > 256 * 1024 {
            return Err(Error::http(400, "千问办公凭据 JSON 过大"));
        }
        serde_json::from_str(raw.trim().trim_start_matches('\u{feff}'))
            .map_err(|_| Error::http(400, "千问办公凭据不是有效 JSON"))?
    } else {
        value.clone()
    };
    if !value.is_object() {
        return Err(Error::http(400, "千问办公凭据必须是 JSON 对象"));
    }
    if value.to_string().len() > 256 * 1024 {
        return Err(Error::http(400, "千问办公凭据 JSON 过大"));
    }
    owner(&value)?;
    for _ in 0..4 {
        let nested = ["secret", "session", "data"]
            .iter()
            .find_map(|key| value.get(*key).filter(|child| child.is_object()).cloned());
        let Some(nested) = nested else {
            break;
        };
        owner(&nested)?;
        let mut next = nested;
        for (key, child) in value.as_object().into_iter().flatten() {
            if ["secret", "session", "data"].contains(&key.as_str()) {
                continue;
            }
            if next
                .get(key)
                .is_none_or(|value| value.is_null() || value == "")
            {
                next[key] = child.clone();
            }
        }
        value = next;
    }
    if let Some(auth) = value["auth"].as_object().cloned() {
        owner(&Value::Object(auth.clone()))?;
        let known_identity = identity_fields(&value, &["userId", "user_id", "uid"], true)?;
        let known_machine =
            identity_fields(&value, &["machine_id", "machineId", "loginDeviceId"], false)?;
        let nested_identity = identity_fields(
            &Value::Object(auth.clone()),
            &["userId", "user_id", "uid"],
            true,
        )?;
        let nested_machine = identity_fields(
            &Value::Object(auth.clone()),
            &["machine_id", "machineId", "loginDeviceId"],
            false,
        )?;
        if !known_identity.is_empty()
            && !nested_identity.is_empty()
            && known_identity != nested_identity
            || !known_machine.is_empty()
                && !nested_machine.is_empty()
                && known_machine != nested_machine
        {
            return Err(
                Error::http(409, "千问办公外层与嵌套凭据身份不一致").with_code("identity_changed")
            );
        }
        for (key, child) in auth {
            if [
                "machine_id",
                "machineId",
                "loginDeviceId",
                "userId",
                "user_id",
                "uid",
                "isBiz",
                "is_biz",
                "teamId",
                "team_id",
                "orgId",
                "organizationId",
            ]
            .contains(&key.as_str())
            {
                if let Some(current) = value
                    .get(&key)
                    .filter(|current| !current.is_null() && *current != "")
                {
                    if !child.is_null() && child != "" && current != &child {
                        return Err(Error::http(409, "千问办公外层与嵌套凭据身份不一致")
                            .with_code("identity_changed"));
                    }
                }
            }
            if value
                .get(&key)
                .is_none_or(|value| value.is_null() || value == "")
            {
                value[&key] = child;
            }
        }
    }
    if let Some(user) = value["user"].as_object().cloned() {
        let id = identity_fields(
            &Value::Object(user.clone()),
            &["id", "userId", "user_id", "uid"],
            true,
        )?;
        let current = identity_fields(&value, &["userId", "user_id", "uid"], true)?;
        if !current.is_empty() && !id.is_empty() && current != id {
            return Err(
                Error::http(409, "千问办公嵌套用户与凭据身份不一致").with_code("identity_changed")
            );
        }
        if current.is_empty() && !id.is_empty() {
            value["userId"] = json!(id);
        }
        for key in [
            "nickname",
            "name",
            "username",
            "email",
            "isBiz",
            "is_biz",
            "teamId",
            "team_id",
            "orgId",
            "organization_id",
            "organizationId",
            "organization",
        ] {
            if value
                .get(key)
                .is_none_or(|value| value.is_null() || value == "")
            {
                if let Some(child) = user.get(key) {
                    value[key] = child.clone();
                }
            }
        }
    }
    if value["desktop"] == true || value["importDesktop"] == true {
        return Err(Error::http(
            400,
            "千问办公国内版不读取本机凭据，请使用设备授权或凭据文本",
        ));
    }
    if ["pat", "personalAccessToken", "personal_token"]
        .iter()
        .any(|key| {
            value
                .get(*key)
                .is_some_and(|value| !value.is_null() && value != "")
        })
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
        if !raw.is_empty()
            && if key == "provider" {
                raw != endpoints::PROVIDER
            } else {
                !["cn", "qwenwork", "qwenworkcn", "qwen-work-cn"]
                    .iter()
                    .any(|alias| raw.eq_ignore_ascii_case(alias))
            }
        {
            return Err(Error::http(400, "当前插件只接受千问办公国内版凭据"));
        }
    }
    // 固定公有云来源，不能把企业自定义域、国际版或其它产品记录默默转投本家。
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
                "千问办公国内版不接受其他服务地址",
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
        let mut user = identity_fields(&value, &["userId", "user_id", "uid"], true)?;
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
        // 存量记录的相对时长不能在每次读取时重新起算，登录/续期入口会一次性固化绝对值。
        if let Some(fields) = lifetime.as_object_mut() {
            fields.remove("expires_in");
            fields.remove("expiresIn");
        }
        let expires = util::token_expiry(&lifetime, &access)?;
        let is_biz = value
            .get("is_biz")
            .or_else(|| value.get("isBiz"))
            .filter(|value| !value.is_null())
            .map(|value| {
                value
                    .as_bool()
                    .ok_or_else(|| Error::http(400, "千问办公账号类型必须为布尔值"))
            })
            .transpose()?;
        let mut organization = identity_fields(
            &value,
            &["orgId", "organization_id", "organizationId"],
            true,
        )?;
        let mut team = identity_fields(&value, &["teamId", "team_id"], true)?;
        if value["organization"].is_object() {
            if organization.is_empty() {
                organization =
                    identity_fields(&value["organization"], &["org_id", "orgId", "id"], true)?;
            }
            if team.is_empty() {
                team = identity_fields(&value["organization"], &["team_id", "teamId"], true)?;
            }
        }
        let mut result = Self {
            access,
            refresh: refresh.clone(),
            expires,
            user,
            machine,
            name: util::text(&value, &["nickname", "name", "username"]),
            email: util::text(&value, &["email"]),
            is_biz,
            organization,
            team,
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
        if self.is_biz == Some(true) && self.team.is_empty() {
            return Err(Error::http(
                400,
                "千问办公企业账号缺少团队身份，请导入完整记录",
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
    pub fn expiring_within(&self, min_lead_ms: i64) -> Result<bool> {
        let now = util::now()?;
        let margin = endpoints::REFRESH_MARGIN.max(min_lead_ms.max(0));
        Ok(self.is_biz != Some(true) && self.can_refresh()
            && self
                .expires
                .is_some_and(|expires| expires <= now.saturating_add(margin)))
    }
    pub fn expiring(&self) -> Result<bool> {
        self.expiring_within(0)
    }
    pub fn validate_public(&self) -> Result<()> {
        util::identity(&self.user)?;
        util::identity(&self.machine)?;
        util::identity(&self.organization)?;
        util::identity(&self.team)?;
        if self.name.chars().count() > 100
            || self.email.chars().count() > 320
            || [&self.name, &self.email]
                .iter()
                .any(|value| value.chars().any(char::is_control))
        {
            return Err(Error::http(400, "千问办公公开账号信息格式无效"));
        }
        let secrets = [&self.access[..], &self.refresh[..], self.oauth_refresh()];
        if [
            &self.name,
            &self.email,
            &self.user,
            &self.machine,
            &self.organization,
            &self.team,
        ]
        .iter()
        .any(|public| {
            secrets
                .iter()
                .any(|secret| !secret.is_empty() && public.contains(*secret))
        }) {
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
        json!({ "provider": endpoints::PROVIDER, "mode": "cn", "accessToken": self.access,
            "refreshToken": nullable(&self.refresh), "expiresAt": self.expires, "userId": self.user,
            "machineId": self.machine, "nickname": nullable(&self.name), "email": nullable(&self.email),
            "isBiz": self.is_biz, "orgId": nullable(&self.organization), "teamId": nullable(&self.team) })
    }
    pub fn same_account(&self, other: &Self) -> bool {
        self.user == other.user
            && self.machine == other.machine
            && self.is_biz == other.is_biz
            && self.organization == other.organization
            && self.team == other.team
    }
    pub fn binding(&self) -> String {
        if self.is_biz != Some(true) && self.organization.is_empty() && self.team.is_empty() {
            return util::hash(&json!([endpoints::PROVIDER, self.user, self.machine]).to_string());
        }
        util::hash(
            &json!([
                endpoints::PROVIDER,
                self.user,
                self.machine,
                self.is_biz,
                self.organization,
                self.team
            ])
            .to_string(),
        )
    }
}
