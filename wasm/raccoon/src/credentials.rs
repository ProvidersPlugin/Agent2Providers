//! 登录态归一：旧JWT可刷新；IAM即便携带残留refreshToken也不使用它。

use super::{endpoints, jwt, util};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

#[derive(Clone)]
pub struct Credential {
    pub token: String,
    pub refresh: String,
    pub expires: Option<i64>,
    pub user: String,
    pub org: String,
}
impl Credential {
    pub fn parse(payload: &Value) -> Result<Self> {
        let token = payload.as_str().map(jwt::strip).unwrap_or_else(|| {
            jwt::strip(&util::text(
                payload,
                &["accessToken", "access_token", "token", "auth_token"],
            ))
        });
        if token.is_empty()
            || token.len() > endpoints::MAX_CREDENTIALS
            || token.chars().any(char::is_whitespace)
        {
            return Err(Error::http(400, "小浣熊缺少有效accessToken，或凭据过大"));
        }
        let claims =
            jwt::claims(&token).ok_or_else(|| Error::http(400, "小浣熊凭据不是有效JWT"))?;
        let iam = jwt::iam(&token);
        let refresh = if iam {
            String::new()
        } else {
            jwt::strip(&util::text(payload, &["refreshToken", "refresh_token"]))
        };
        let supplied = util::text(payload, &["userId"]);
        let extracted = jwt::uid(&claims);
        let user = if iam && !supplied.is_empty() {
            supplied
        } else if extracted.is_empty() {
            "unknown".into()
        } else {
            extracted
        };
        let org = util::text(payload, &["officeIdentity", "office_identity"]);
        let org = if org == "personal" {
            String::new()
        } else {
            org
        };
        if org.len() > 256 || !org.is_ascii() || org.bytes().any(|byte| byte < 32 || byte == 127) {
            return Err(Error::http(400, "小浣熊组织身份格式无效"));
        }
        Ok(Self {
            expires: jwt::expiry(&token)
                .or_else(|| util::integer(&payload["expiresAt"]))
                .or_else(|| util::integer(&payload["tokenExpiresAt"])),
            token,
            refresh,
            user,
            org,
        })
    }
    pub fn expiring(&self) -> bool {
        // 只有真正能续期的来源才提前五分钟处理；单 accessToken 在实际到期前仍可使用。
        let margin = if self.can_refresh() { 300_000 } else { 0 };
        self.expires
            .is_some_and(|at| at.saturating_sub(margin) <= sdk::now())
    }
    pub fn can_refresh(&self) -> bool {
        !self.refresh.is_empty() && !jwt::iam(&self.token)
    }
    pub fn same(&self, other: &Self) -> bool {
        self.token == other.token
            && self.refresh == other.refresh
            && self.org == other.org
            && self.user == other.user
    }
    pub fn stamp(&self) -> String {
        util::hash(
            &json!({ "refresh": self.refresh, "org": self.org }).to_string(),
        )
    }
    pub fn secret(&self) -> Value {
        let mut value = json!({ "accessToken": self.token, "userId": self.user,
            "officeIdentity": if self.org.is_empty() { "personal" } else { &self.org } });
        if !self.refresh.is_empty() && !jwt::iam(&self.token) {
            value["refreshToken"] = json!(self.refresh);
        }
        if let Some(at) = self.expires {
            value["expiresAt"] = json!(at);
        }
        value
    }
}
