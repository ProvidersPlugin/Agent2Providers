//! 两层凭据分别使用：模型 Basic 不预测到期，STS 临时 AK/SK 只服务控制面。

use super::{endpoints, signer, util::text};
use agent2api_wasm_sdk::{self as sdk, Error, Result, Value};

pub const MARGIN_MS: i64 = 30 * 60 * 1000;

#[derive(Clone, Default)]
pub struct Credentials {
    pub base_url: String,
    pub model_app_key: String,
    pub model_app_secret: String,
    pub signing: signer::Credential,
    pub expires_at: i64,
}

impl Credentials {
    pub fn from_secret(secret: &Value) -> Self {
        Self {
            base_url: text(secret, &["baseUrl", "base_url", "api_base", "apiBase"]),
            model_app_key: text(secret, &["modelAppKey", "appKey", "model_app_key"]),
            model_app_secret: text(secret, &["modelAppSecret", "appSecret", "model_app_secret"]),
            signing: signer::Credential {
                access_key_id: text(secret, &["accessKeyId", "access_key_id", "accessKey"]),
                secret_access_key: text(
                    secret,
                    &["secretAccessKey", "secret_access_key", "secretKey"],
                ),
                security_token: text(secret, &["securityToken", "security_token"]),
                project_id: text(secret, &["projectId", "project_id"]),
            },
            expires_at: secret["expiresAt"].as_i64().unwrap_or(0),
        }
    }
    pub fn forwarding(&self) -> Result<()> {
        if self.base_url.is_empty()
            || self.model_app_key.is_empty()
            || self.model_app_secret.is_empty()
        {
            return Err(Error::http(401, "OfficeAce 缺少模型网关基址或 Basic 凭据"));
        }
        if self.base_url.len() > 2048
            || self.model_app_key.len() > 8192
            || self.model_app_secret.len() > 8192
            || self.base_url.contains('\0')
            || self.model_app_key.contains(':')
            || self.model_app_key.chars().any(char::is_control)
            || self.model_app_secret.chars().any(char::is_control)
        {
            return Err(Error::http(400, "OfficeAce 模型凭据长度或格式无效"));
        }
        endpoints::official_base(&self.base_url)?;
        Ok(())
    }
    pub fn has_control(&self) -> bool {
        !self.signing.access_key_id.is_empty() && !self.signing.secret_access_key.is_empty()
    }
    pub fn control(&self) -> Result<()> {
        if !self.has_control() {
            return Err(Error::http(
                400,
                "OfficeAce 未配置控制面临时凭据（AK/SK）；对话仍可用，请网页登录补齐余额与签到授权",
            )
            .with_code("usage_not_configured"));
        }
        if self.signing.security_token.is_empty() {
            return Err(Error::http(
                400,
                "OfficeAce 缺少临时安全令牌，请重新登录补齐授权",
            ));
        }
        Ok(())
    }
    pub fn expiring(&self) -> bool {
        self.expires_at > 0 && self.expires_at.saturating_sub(MARGIN_MS) <= sdk::now()
    }
    pub fn same_model(&self, other: &Self) -> bool {
        self.base_url == other.base_url
            && self.model_app_key == other.model_app_key
            && self.model_app_secret == other.model_app_secret
    }
    pub fn same_control(&self, other: &Self) -> bool {
        self.signing.access_key_id == other.signing.access_key_id
            && self.signing.secret_access_key == other.signing.secret_access_key
            && self.signing.security_token == other.signing.security_token
            && self.expires_at == other.expires_at
    }
    pub fn secrets(&self) -> [&str; 5] {
        [
            &self.model_app_key,
            &self.model_app_secret,
            &self.signing.access_key_id,
            &self.signing.secret_access_key,
            &self.signing.security_token,
        ]
    }
}

pub fn can_refresh(secret: &Value) -> bool {
    !text(secret, &["refreshToken", "refresh_token"]).is_empty()
        && super::dpop::DpopKey::from_json(&secret["dpopKeyPair"]).is_ok()
}
