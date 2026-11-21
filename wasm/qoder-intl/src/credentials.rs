//! Qoder 国际版 dt/drt、jt/jrt 与 PAT 打包格式；provider 固定，不从 mode 选择站点。
//! 无地区字段的旧记录只在宿主已限定的国际入口读取，绝不据此查找或覆盖国内账号。
use super::{endpoints, util};
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
}
pub fn decode(value: &Value) -> Result<Value> {
    let mut value = if let Some(raw) = value.as_str() {
        if raw.len() > 256 * 1024 {
            return Err(Error::http(400, "Qoder 凭据 JSON 过大"));
        }
        serde_json::from_str(raw).map_err(|_| Error::http(400, "Qoder 凭据不是有效 JSON"))?
    } else {
        value.clone()
    };
    if !value.is_object() {
        return Err(Error::http(400, "Qoder 凭据必须是 JSON 对象"));
    }
    owner(&value)?;
    if let Some(auth) = value["auth"].as_object().cloned() {
        owner(&Value::Object(auth.clone()))?;
        for (key, child) in auth {
            if value
                .get(&key)
                .is_none_or(|value| value.is_null() || value == "")
            {
                value[&key] = child;
            }
        }
    }
    if value["desktop"] == true || value["importDesktop"] == true {
        return Err(Error::http(
            400,
            "当前主程序尚未接通 Qoder 国际版本机凭据导入，请使用设备授权或粘贴凭据",
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
            .ok_or_else(|| Error::http(400, "Qoder 地区标记格式无效"))?
            .trim();
        let compatible =
            key != "provider" && matches!(raw.to_ascii_lowercase().as_str(), "global" | "intl");
        if !raw.is_empty() && raw != endpoints::PROVIDER && !compatible {
            return Err(Error::http(400, "当前插件只接受 Qoder 国际版凭据"));
        }
    }
    Ok(())
}
impl Credentials {
    pub fn parse(value: &Value) -> Result<Self> {
        let value = decode(value)?;
        let access = util::secret(&value, &["accessToken", "access", "token", "access_token"])?;
        if access.is_empty() || access.contains('|') {
            return Err(Error::http(400, "Qoder 缺少有效访问凭据"));
        }
        let refresh = util::secret(&value, &["refreshToken", "refresh", "refresh_token"])?;
        let parts = refresh.split('|').collect::<Vec<_>>();
        let (packed_user, packed_machine) = match parts.as_slice() {
            ["pat", pat, _, user, machine] if !pat.is_empty() => (*user, *machine),
            [_, user, machine] => (*user, *machine),
            [_] => ("", ""),
            _ => {
                return Err(Error::http(
                    400,
                    "Qoder 刷新凭据格式无效，请粘贴完整账号 JSON",
                ))
            }
        };
        let mut user = util::text(&value, &["userId", "user_id", "uid"]);
        let mut machine = util::text(&value, &["machineId", "machine_id"]);
        if !user.is_empty() && !packed_user.is_empty() && user != packed_user
            || !machine.is_empty() && !packed_machine.is_empty() && machine != packed_machine
        {
            return Err(Error::http(409, "Qoder 凭据中的用户或设备标识不一致")
                .with_code("identity_changed"));
        }
        if user.is_empty() {
            user = packed_user.into();
        }
        if machine.is_empty() {
            machine = packed_machine.into();
        }
        util::identity(&user)?;
        util::identity(&machine)?;
        let mut expires = None;
        for key in [
            "expiresAt",
            "expires",
            "expires_at",
            "tokenExpiresAt",
            "expire_time",
            "expireTime",
        ] {
            if let Some(raw) = value
                .get(key)
                .filter(|raw| !raw.is_null() && *raw != "" && *raw != 0)
            {
                let at = util::timestamp(raw)
                    .ok_or_else(|| Error::http(400, "Qoder 到期时间必须是有效时间戳或 ISO 8601"))?;
                expires = Some(at);
                break;
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
                "Qoder 缺少用户标识，请重新授权或粘贴完整账号记录",
            ));
        }
        util::identity(&self.user)?;
        // 普通 COSY machineId 不是 UMID，账号落地后始终复用；活动不能拿它冒充三件套。
        if self.machine.is_empty() {
            self.machine = match agent2api_wasm_sdk::client_device::machine_id(endpoints::PROVIDER)? {
                Some(value) => value,
                None => util::uuid()?,
            };
        }
        util::identity(&self.machine)?;
        if !self.refresh.is_empty() {
            let parts = self.refresh.split('|').collect::<Vec<_>>();
            let prefix = if parts.len() >= 3 {
                parts[..parts.len() - 2].join("|")
            } else {
                self.refresh.clone()
            };
            self.refresh = format!("{prefix}|{}|{}", self.user, self.machine);
        }
        if self.refresh.len() > endpoints::MAX_TOKEN {
            return Err(Error::http(400, "Qoder 刷新凭据过长"));
        }
        self.validate_public()
    }
    pub fn pat(&self) -> Option<&str> {
        self.refresh
            .strip_prefix("pat|")?
            .split('|')
            .next()
            .filter(|value| !value.is_empty())
    }
    pub fn oauth_refresh(&self) -> &str {
        if self.pat().is_some() {
            ""
        } else {
            self.refresh.split('|').next().unwrap_or("")
        }
    }
    pub fn can_refresh(&self) -> bool {
        self.pat().is_some() || !self.oauth_refresh().is_empty()
    }
    pub fn expiring_within(&self, min_lead_ms: i64) -> Result<bool> {
        let now = util::now()?;
        let margin = endpoints::REFRESH_MARGIN.max(min_lead_ms.max(0));
        Ok(self.can_refresh()
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
        if self.name.chars().count() > 100
            || self.email.chars().count() > 320
            || [&self.name, &self.email]
                .iter()
                .any(|value| value.chars().any(char::is_control))
        {
            return Err(Error::http(400, "Qoder 公开账号信息格式无效"));
        }
        let parts = self.refresh.split('|').collect::<Vec<_>>();
        let mut secrets = vec![self.access.as_str(), self.refresh.as_str()];
        match parts.as_slice() {
            ["pat", pat, refresh, _, _] => {
                secrets.push(*pat);
                secrets.push(*refresh);
            }
            [refresh, _, _] => secrets.push(*refresh),
            [refresh] => secrets.push(*refresh),
            _ => {}
        }
        if [&self.name, &self.email, &self.user, &self.machine]
            .iter()
            .any(|public| {
                secrets
                    .iter()
                    .any(|secret| !secret.is_empty() && public.contains(*secret))
            })
        {
            return Err(Error::http(400, "Qoder 公开账号字段不能包含完整凭据"));
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
            "machineId": self.machine, "nickname": nullable(&self.name), "email": nullable(&self.email) })
    }
    pub fn binding(&self) -> String {
        util::hash(&json!([endpoints::PROVIDER, self.user, self.machine]).to_string())
    }
}
