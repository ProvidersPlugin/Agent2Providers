//! Kiro 凭证：账号里存什么、怎么读、怎么给界面看。
//!
//! ── 三种凭证来源 ────────────────────────────────────────────
//! ```text
//!   apikey   ksk_…（官方 API key，tokentype: API_KEY）
//!   oauth    登录时换到的 access/refresh（social / idc / external-idp）
//!   本机登录态  kiro-cli 的 SQLite 或 Kiro IDE 的 kiro-auth-token.json
//!              —— 账号里只记来源（source），token 每次实时读
//! ```
//!
//! ── 为什么本机登录态不落快照 ────────────────────────────────
//! 与 Raccoon「复用桌面端登录态」同一条理由：客户端会自己续期、重新登录，
//! 落一份快照会让网关拿着旧 token 直到用户手动重加账号。实时读的代价是
//! 每个请求多一次磁盘读（SQLite / 小 JSON），可以忽略。
//!
//! ── 公开视图里绝不放 token ──────────────────────────────────
//! `public` 只写名字、uid、令牌尾 4 位、有效期与套餐。`secret` 才是凭证本体
//! （网关的公开投影会整块剥掉它）。

use serde_json::{json, Value};

use super::endpoints;

/// 一个账号的凭证（从 `secret` JSON 读出来的形态）。
#[derive(Clone, Debug, Default)]
pub struct Credentials {
    /// `apikey` / `social` / `idc` / `external-idp`
    pub method: String,
    pub access: String,
    pub refresh: String,
    /// 毫秒时间戳；0 = 不知道
    pub expires: i64,
    pub region: String,
    pub profile: String,
    pub client_id: String,
    pub client_secret: String,
    /// external-idp 的 token 端点
    pub token_url: String,
    /// google / github / builderid / enterprise / internal
    pub login_provider: String,
    /// 登录方式（与 login_provider 同值，落盘名沿用参考实现的 `uid`）
    pub uid: String,
    pub account_id: String,
    pub plan: String,
    /// `kiro-cli` / `kiro-ide` / 空
    pub source: String,
    /// kiro-cli 的 auth_kv 键（写回用）
    pub db_key: String,
    /// Kiro IDE 的 token 文件路径（写回用）
    pub ide_path: String,
    /// 账号备注名
    pub name: String,
}

fn text(secret: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = secret.get(*key).and_then(Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    String::new()
}

fn number(secret: &Value, keys: &[&str]) -> i64 {
    for key in keys {
        if let Some(value) = secret.get(*key).and_then(Value::as_i64) {
            if value > 0 {
                return value;
            }
        }
        if let Some(value) = secret.get(*key).and_then(Value::as_str) {
            if let Ok(parsed) = value.trim().parse::<i64>() {
                if parsed > 0 {
                    return parsed;
                }
            }
        }
    }
    0
}

impl Credentials {
    /// 从账号的 `secret` 读一份（宽容读取：字段名兼容旧记录）。
    pub fn from_secret(secret: &Value) -> Self {
        let method = text(secret, &["method", "authMethod"]);
        let access = text(secret, &["access", "accessToken", "token", "apiKey", "key"]);
        let method = if method.is_empty() {
            if access.starts_with("ksk_") {
                "apikey".to_string()
            } else {
                "social".to_string()
            }
        } else {
            method
        };
        let uid = text(secret, &["uid", "loginProvider"]);
        Self {
            region: {
                let region = text(secret, &["region"]);
                if region.is_empty() {
                    "us-east-1".to_string()
                } else {
                    region
                }
            },
            method,
            access,
            refresh: text(secret, &["refresh", "refreshToken"]),
            expires: number(secret, &["expires", "expiresAt"]),
            profile: text(secret, &["profile", "profileArn"]),
            client_id: text(secret, &["clientId"]),
            client_secret: text(secret, &["clientSecret"]),
            token_url: text(secret, &["tokenURL", "tokenUrl"]),
            login_provider: text(secret, &["loginProvider"]),
            uid,
            account_id: text(secret, &["accountId", "email"]),
            plan: text(secret, &["plan", "subscriptionTitle"]),
            source: text(secret, &["source"]),
            db_key: text(secret, &["dbKey"]),
            ide_path: text(secret, &["idePath"]),
            name: text(secret, &["name"]),
        }
    }

    /// 写回 `secret`（保留未知字段：整库备份 / 云同步不该丢东西）。
    pub fn write_into(&self, secret: &mut Value) {
        let Some(object) = secret.as_object_mut() else {
            *secret = json!({});
            return;
        };
        object.insert("method".into(), Value::String(self.method.clone()));
        object.insert("access".into(), Value::String(self.access.clone()));
        object.insert("refresh".into(), Value::String(self.refresh.clone()));
        object.insert("expires".into(), json!(self.expires));
        object.insert("region".into(), Value::String(self.region.clone()));
        object.insert("profileArn".into(), Value::String(self.profile.clone()));
        object.insert("clientId".into(), Value::String(self.client_id.clone()));
        object.insert(
            "clientSecret".into(),
            Value::String(self.client_secret.clone()),
        );
        if !self.token_url.is_empty() {
            object.insert("tokenURL".into(), Value::String(self.token_url.clone()));
        }
        object.insert(
            "loginProvider".into(),
            Value::String(self.login_provider.clone()),
        );
        object.insert("uid".into(), Value::String(self.uid.clone()));
        object.insert("accountId".into(), Value::String(self.account_id.clone()));
        if !self.plan.is_empty() {
            object.insert("plan".into(), Value::String(self.plan.clone()));
        }
        if !self.source.is_empty() {
            object.insert("source".into(), Value::String(self.source.clone()));
        }
        if !self.db_key.is_empty() {
            object.insert("dbKey".into(), Value::String(self.db_key.clone()));
        }
        if !self.ide_path.is_empty() {
            object.insert("idePath".into(), Value::String(self.ide_path.clone()));
        }
    }

    /// 是 API key（不刷新、不写回本机）。
    pub fn is_api_key(&self) -> bool {
        self.method == "apikey"
    }

    /// 是「实时读本机登录态」的账号。
    pub fn is_local(&self) -> bool {
        !self.source.is_empty()
    }

    /// `tokentype` 头（API key 与外部 IdP 才有）。
    pub fn token_type(&self) -> Option<&'static str> {
        match self.method.as_str() {
            "apikey" => Some("API_KEY"),
            "external-idp" => Some("EXTERNAL_IDP"),
            _ => None,
        }
    }

    /// 实际使用的地区（profile 优先，登录地区兜底）。
    pub fn region(&self) -> String {
        endpoints::region_of(&self.profile, &self.region)
    }

    /// 凭证现在可用（离过期还早 / 永不过期）。
    pub fn fresh(&self, early_ms: i64) -> bool {
        self.is_api_key() || self.expires == 0 || self.expires - now_ms() > early_ms
    }
}

/// 当前毫秒时间戳。
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// 令牌尾 4 字符（多字节安全）。
pub fn tail_of(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let start = chars.len().saturating_sub(4);
    chars[start..].iter().collect()
}

/// 公开视图（界面读的键名沿用既有契约）。
pub fn public(secret: &Value) -> Value {
    let credentials = Credentials::from_secret(secret);
    let name = if !credentials.name.is_empty() {
        credentials.name.clone()
    } else if !credentials.account_id.is_empty() {
        credentials.account_id.clone()
    } else if credentials.is_api_key() {
        "Kiro API".to_string()
    } else if credentials.source == "kiro-cli" {
        "kiro-cli 的账号".to_string()
    } else if credentials.source == "kiro-ide" {
        "Kiro IDE 的账号".to_string()
    } else {
        "Kiro".to_string()
    };
    let mut view = json!({
        "name": name,
        "accountId": credentials.account_id.clone(),
        "uid": credentials.uid.clone(),
        "tokenTail": tail_of(&credentials.access),
        "hasCredentials": !credentials.access.is_empty() || credentials.is_local(),
    });
    if !credentials.plan.is_empty() {
        view["plan"] = json!(credentials.plan);
    }
    if credentials.expires > 0 {
        view["expiresAt"] = json!(credentials.expires);
    }
    if credentials.is_api_key() {
        view["longLived"] = json!(true);
    }
    if !credentials.source.is_empty() {
        view["desktop"] = json!(true);
        view["source"] = json!(credentials.source);
    }
    view
}

/// 账号的稳定身份（去重）：API key 按 key 的哈希，其余按邮箱+登录方式。
pub fn identity(secret: &Value) -> String {
    use sha2::{Digest, Sha256};
    let credentials = Credentials::from_secret(secret);
    if credentials.is_api_key() {
        let digest = Sha256::digest(credentials.access.as_bytes());
        let hex: String = digest
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        return format!("kiro:key:{hex}");
    }
    if !credentials.source.is_empty() {
        return format!("kiro:{}", credentials.source);
    }
    if !credentials.account_id.is_empty() {
        let way = if credentials.uid.is_empty() {
            "own"
        } else {
            credentials.uid.as_str()
        };
        return format!("kiro:{}:{way}", credentials.account_id.to_lowercase());
    }
    String::new()
}
