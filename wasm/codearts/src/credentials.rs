//! 本仓 CodeArts 凭据格式的受控版本（AGPL-3.0-only，2026-11-10）。
//! 只读本次宿主传入对象，不读环境/文件，不把手工导入当联网验证。

use agent2api_wasm_sdk::{json, Result, Value};
use serde::{Deserialize, Serialize};

use crate::{dpop, errors, util};

const MAX_SECRET: usize = 8192;
/// 网页登录与缺少客户端标识的旧凭据使用的默认值。
pub const DEFAULT_CLIENT_ID: &str = "vscode-codebot";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RefreshMaterial {
    MissingToken,
    MissingContext,
    InvalidClient,
    InvalidVerifier,
    InvalidProofKey,
    Complete,
}

impl RefreshMaterial {
    pub fn name(self) -> &'static str {
        match self {
            Self::MissingToken => "missing_refresh_token",
            Self::MissingContext => "missing_oauth_context",
            Self::InvalidClient => "invalid_client_id",
            Self::InvalidVerifier => "invalid_pkce_verifier",
            Self::InvalidProofKey => "invalid_dpop_key",
            Self::Complete => "complete",
        }
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Jwk {
    #[serde(default)]
    pub kty: String,
    #[serde(default)]
    pub crv: String,
    #[serde(default)]
    pub x: String,
    #[serde(default)]
    pub y: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub d: String,
}
#[derive(Clone, Default, Deserialize, Serialize)]
pub struct DpopPair {
    #[serde(default)]
    pub private_key_jwk: Jwk,
    #[serde(default)]
    pub public_key_jwk: Jwk,
}
#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Pkce {
    #[serde(default)]
    pub code_verifier: String,
    #[serde(default)]
    pub code_challenge: String,
    #[serde(default)]
    pub code_challenge_method: String,
}
#[derive(Clone, Default, Deserialize, Serialize)]
pub struct OAuthContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default)]
    pub pkce_pair: Pkce,
    #[serde(default)]
    pub dpop_key_pair: DpopPair,
}

impl OAuthContext {
    /// 网页登录与缺少客户端标识的旧凭据沿用默认客户端标识。
    pub fn effective_client_id(&self) -> &str {
        self.client_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_CLIENT_ID)
    }
}
#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Credential {
    #[serde(default)]
    pub access_key_id: String,
    #[serde(default)]
    pub secret_access_key: String,
    #[serde(default)]
    pub security_token: String,
    #[serde(default)]
    pub expires_at: String,
    #[serde(default)]
    pub domain_id: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub user_name: String,
    #[serde(default)]
    pub login_type: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_context: Option<OAuthContext>,
}

pub fn own_provider(value: &Value) -> Result<()> {
    if let Some(provider) = value.get("provider").filter(|value| !value.is_null()) {
        if provider.as_str() != Some("codearts") {
            return Err(errors::invalid("这份凭据不属于 CodeArts，未保存或合并账号"));
        }
    }
    Ok(())
}

pub fn payload(value: &Value) -> Result<Value> {
    let value = if let Some(text) = value.as_str() {
        util::object(text)?
    } else {
        value.clone()
    };
    if !value.is_object() || value.to_string().len() > 128 * 1024 {
        return Err(errors::invalid(
            "CodeArts 需要一份不超过 128 KiB 的单账号凭据 JSON",
        ));
    }
    own_provider(&value)?;
    // 严格保留原生的两个嵌套入口；不识别文件路径、客户端来源或任意 secret 包装。
    let containers: Vec<_> = ["codearts_provider_credential", "credential"]
        .iter()
        .filter_map(|name| value.get(*name))
        .collect();
    if containers.len() > 1 {
        return Err(errors::invalid(
            "CodeArts 凭据含多份包装，未选择或合并其中一份",
        ));
    }
    let body = containers.first().copied().unwrap_or(&value);
    if !body.is_object() {
        return Err(errors::invalid("CodeArts 凭据内层必须是对象"));
    }
    own_provider(body)?;
    if !containers.is_empty() {
        for name in [
            "access_key_id",
            "secret_access_key",
            "security_token",
            "refresh_token",
            "oauth_context",
            "domain_id",
            "user_id",
        ] {
            if value
                .get(name)
                .is_some_and(|field| Some(field) != body.get(name))
            {
                return Err(errors::invalid(
                    "CodeArts 内外凭据字段冲突，未拼接多份授权材料",
                ));
            }
        }
    }
    Ok(body.clone())
}

impl Credential {
    pub fn parse(value: &Value) -> Result<Self> {
        let body = payload(value)?;
        let mut c: Self = serde_json::from_value(body.clone()).map_err(|_| {
            errors::invalid("CodeArts 凭据字段类型无效；登录材料需要字符串及原有嵌套对象")
        })?;
        if c.expires_at.trim().is_empty() {
            c.expires_at = util::text(&body, &["expiration", "expires_at", "expiresAt"]);
        }
        if c.domain_id.trim().is_empty() {
            c.domain_id = util::text(&body, &["domain_id", "account_id"]);
        }
        if c.user_id.trim().is_empty() {
            c.user_id = util::text(&body, &["user_id", "principal_id"]);
        }
        if c.user_name.trim().is_empty() {
            c.user_name = util::text(&body, &["user_name", "principal_urn"]);
        }
        if c.login_type.trim().is_empty() {
            c.login_type = "WEB".into();
        }
        for field in [
            &mut c.access_key_id,
            &mut c.secret_access_key,
            &mut c.security_token,
            &mut c.expires_at,
            &mut c.domain_id,
            &mut c.user_id,
            &mut c.user_name,
            &mut c.login_type,
            &mut c.refresh_token,
        ] {
            *field = field.trim().to_string();
        }
        if c.access_key_id.is_empty() || c.secret_access_key.is_empty() {
            return Err(errors::invalid(
                "CodeArts 凭据缺少 access_key_id 或 secret_access_key",
            ));
        }
        for text in [
            &c.access_key_id,
            &c.secret_access_key,
            &c.security_token,
            &c.refresh_token,
        ] {
            if text.len() > MAX_SECRET || text.chars().any(char::is_control) {
                return Err(errors::invalid(
                    "CodeArts 凭据过长或包含控制字符，未公开其内容",
                ));
            }
        }
        for header in [&c.access_key_id, &c.security_token] {
            if header.bytes().any(|byte| !(0x21..=0x7e).contains(&byte)) {
                return Err(errors::invalid("CodeArts 请求鉴权材料含有不合法的头部字符"));
            }
        }
        for text in [
            &c.domain_id,
            &c.user_id,
            &c.user_name,
            &c.login_type,
            &c.expires_at,
        ] {
            if text.len() > 1024 || text.chars().any(char::is_control) {
                return Err(errors::invalid("CodeArts 身份或到期字段格式无效"));
            }
        }
        if let Some(context) = &mut c.oauth_context {
            if let Some(client) = &mut context.client_id {
                *client = client.trim().to_string();
            }
            context.pkce_pair.code_verifier = context.pkce_pair.code_verifier.trim().to_string();
            context.pkce_pair.code_challenge = context.pkce_pair.code_challenge.trim().to_string();
            context.pkce_pair.code_challenge_method =
                context.pkce_pair.code_challenge_method.trim().to_string();
            if serde_json::to_vec(context)
                .map_err(|_| errors::invalid("CodeArts 长期材料无法保存"))?
                .len()
                > 32 * 1024
            {
                return Err(errors::invalid("CodeArts 长期授权材料过大"));
            }
        }
        // 通用镜像不允许与原生字段互相矛盾，防止显示与实际出站使用两套身份。
        check_alias(&body, "accessToken", &c.access_key_id)?;
        check_alias(&body, "refreshToken", &c.refresh_token)?;
        check_alias(&body, "userId", &c.user_id)?;
        Ok(c)
    }

    pub fn expiry(&self) -> Option<i64> {
        chrono::DateTime::parse_from_rfc3339(&self.expires_at)
            .ok()
            .map(|time| time.timestamp_millis())
            .filter(|at| *at > 0)
    }

    pub fn require_readable(&self, now: i64) -> Result<()> {
        if self
            .domain_id
            .bytes()
            .any(|byte| !(0x21..=0x7e).contains(&byte))
        {
            return Err(errors::invalid(
                "CodeArts 域身份无法安全放入签名请求头，未发请求",
            ));
        }
        if self.security_token.is_empty() {
            return Err(errors::invalid(
                "CodeArts 只读查询还需要 security_token；已保存的 AK/SK 不等于完整临时登录态",
            ));
        }
        match self.expiry() {
            None => Err(errors::invalid(
                "CodeArts 凭据缺少可确认的 RFC3339 到期时间；请完整导入，未猜测有效期限或自动续期",
            )),
            Some(at) if at <= now => Err(agent2api_wasm_sdk::Error::http(
                401,
                "CodeArts 临时凭据已到期，请从主程序续期入口更新；缺少原授权材料时需重新登录",
            )
            .with_code("codearts_access_expired")),
            Some(_) => Ok(()),
        }
    }

    pub fn stable_identity(&self) -> Option<String> {
        // 原生以 '-' 拼接；仅明确无分隔歧义的完整 domain/user 才当跨续期身份。
        let component = |text: &str| {
            !text.is_empty()
                && text.len() <= 128
                && text
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        };
        if !(component(&self.domain_id) && component(&self.user_id)) {
            return None;
        }
        let secrets = self.secrets();
        let identity = format!("codearts-{}-{}", self.domain_id, self.user_id);
        (![&self.domain_id, &self.user_id, &identity]
            .iter()
            .any(|value| errors::contains_secret(value, &secrets)))
        .then_some(identity)
    }

    pub fn snapshot_identity(&self) -> String {
        format!(
            "codearts-snapshot-{}",
            util::digest(&[
                self.domain_id.as_bytes(),
                self.user_id.as_bytes(),
                self.access_key_id.as_bytes(),
                self.secret_access_key.as_bytes(),
                self.security_token.as_bytes()
            ])
        )
    }

    pub fn identity(&self) -> String {
        self.stable_identity()
            .unwrap_or_else(|| self.snapshot_identity())
    }

    /// 与原生同一15分钟调用口径；没有有效期不假装永久有效，非空坏时间戳视为需刷新。
    pub fn needs_refresh(&self, lead_ms: i64, now: i64) -> bool {
        if self.security_token.is_empty() || self.expires_at.is_empty() {
            return false;
        }
        self.expiry()
            .is_none_or(|at| at <= now.saturating_add(lead_ms.max(0)))
    }

    pub fn refresh_material(&self) -> RefreshMaterial {
        if self.refresh_token.is_empty() {
            return RefreshMaterial::MissingToken;
        }
        let Some(context) = &self.oauth_context else {
            return RefreshMaterial::MissingContext;
        };
        // 旧版凭据可能未保存 client_id，缺省时按与原生一致的默认值回退；显式填写的值仍需合法。
        let client = context.effective_client_id();
        if client.len() > 128
            || client
                .chars()
                .any(|ch| ch.is_control() || ch.is_whitespace())
        {
            return RefreshMaterial::InvalidClient;
        }
        let verifier = context.pkce_pair.code_verifier.trim();
        if !(43..=128).contains(&verifier.len())
            || !verifier
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(&byte))
        {
            return RefreshMaterial::InvalidVerifier;
        }
        let challenge_method = context.pkce_pair.code_challenge_method.trim();
        if !challenge_method.is_empty() && !matches!(challenge_method, "SHA-256" | "S256") {
            return RefreshMaterial::InvalidVerifier;
        }
        let challenge = context.pkce_pair.code_challenge.trim();
        if !challenge.is_empty() {
            use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
            use sha2::{Digest, Sha256};
            if URL_SAFE_NO_PAD
                .decode(challenge.trim_end_matches('='))
                .ok()
                .is_none_or(|decoded| {
                    decoded.as_slice() != Sha256::digest(verifier.as_bytes()).as_slice()
                })
            {
                return RefreshMaterial::InvalidVerifier;
            }
        }
        if dpop::validate(&context.dpop_key_pair).is_err() {
            return RefreshMaterial::InvalidProofKey;
        }
        RefreshMaterial::Complete
    }

    pub fn refresh_material_complete(&self) -> bool {
        self.refresh_material() == RefreshMaterial::Complete
    }

    pub fn secrets(&self) -> Vec<String> {
        let mut values = vec![
            self.access_key_id.clone(),
            self.secret_access_key.clone(),
            self.security_token.clone(),
            self.refresh_token.clone(),
        ];
        if let Some(context) = &self.oauth_context {
            values.push(context.pkce_pair.code_verifier.clone());
            values.push(context.pkce_pair.code_challenge.clone());
            for key in [
                &context.dpop_key_pair.private_key_jwk,
                &context.dpop_key_pair.public_key_jwk,
            ] {
                values.extend([key.d.clone(), key.x.clone(), key.y.clone()]);
            }
        }
        values
            .into_iter()
            .flat_map(|value| errors::spellings(&value))
            .collect()
    }

    pub fn secret(&self) -> Result<Value> {
        let mut value =
            serde_json::to_value(self).map_err(|_| errors::invalid("CodeArts 凭据无法保存"))?;
        if let Some(fields) = value.as_object_mut() {
            fields.retain(|_, value| {
                !value.is_null() && value.as_str().is_none_or(|text| !text.is_empty())
            });
        }
        value["accessToken"] = json!(self.access_key_id);
        if !self.refresh_token.is_empty() {
            value["refreshToken"] = json!(self.refresh_token);
        }
        if !self.user_id.is_empty() {
            value["userId"] = json!(self.user_id);
        }
        value["expiresAt"] = json!(self.expiry());
        Ok(value)
    }
}

fn check_alias(value: &Value, name: &str, expected: &str) -> Result<()> {
    if let Some(alias) = value.get(name).filter(|value| !value.is_null()) {
        if alias
            .as_str()
            .is_none_or(|text| !text.trim().is_empty() && text.trim() != expected)
        {
            return Err(errors::invalid(
                "CodeArts 通用字段与原生凭据字段不一致，未选择或合并两份身份",
            ));
        }
    }
    Ok(())
}
