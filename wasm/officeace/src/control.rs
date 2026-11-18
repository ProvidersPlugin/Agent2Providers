//! 控制面上下文只持有一把账号锁，续期/领奖励/零元订单不嵌套锁。
//! 只在明确 APIG 临时凭据过期时刷新一次；403/签名问题不能当过期，模型面不走这里。

use super::{
    account, client,
    credentials::{self, Credentials},
    errors, oauth_gateway, refresh, util,
};
use agent2api_wasm_sdk::{self as sdk, AccountLock, Result, Value};

pub struct Context {
    pub record: Value,
    pub credential: Credentials,
    _lock: AccountLock,
}

impl Context {
    pub fn acquire() -> Result<Self> {
        let (before, rejected) = account::snapshot()?;
        let token = util::text(&before["secret"], &["refreshToken", "refresh_token"]);
        let lock = AccountLock::acquire()?;
        let (record, credential) = account::snapshot()?;
        if record["id"] != before["id"]
            || record["identity"] != before["identity"]
            || !credential.same_model(&rejected)
        {
            return Err(super::errors::account_changed());
        }
        let mut context = Self {
            record,
            credential,
            _lock: lock,
        };
        if context.credential.expiring() && credentials::can_refresh(&context.record["secret"]) {
            if let Err(error) = context.renew_from(&rejected, &token) {
                if context.credential.expires_at <= sdk::now() {
                    return Err(error);
                }
            }
        }
        Ok(context)
    }

    pub fn refresh() -> Result<String> {
        let (before, rejected) = account::snapshot()?;
        let token = util::text(&before["secret"], &["refreshToken", "refresh_token"]);
        let lock = AccountLock::acquire()?;
        let (record, credential) = account::snapshot()?;
        if record["id"] != before["id"]
            || record["identity"] != before["identity"]
            || !credential.same_model(&rejected)
        {
            return Err(super::errors::account_changed());
        }
        let mut context = Self {
            record,
            credential,
            _lock: lock,
        };
        context.renew_from(&rejected, &token)?;
        Ok(context.credential.signing.access_key_id)
    }

    fn renew_from(&mut self, rejected: &Credentials, token: &str) -> Result<()> {
        let record = refresh::locked(self.record.clone(), rejected, token)?;
        self.credential = Credentials::from_secret(&record["secret"]);
        self.record = record;
        Ok(())
    }

    fn renew(&mut self) -> Result<()> {
        let rejected = self.credential.clone();
        let token = util::text(&self.record["secret"], &["refreshToken", "refresh_token"]);
        // 锁内仍回读：其他账号编辑操作不使用这把插件锁，不能拿旧 secret 覆盖。
        self.record = account::same_model(&self.record)?;
        self.renew_from(&rejected, &token)
    }

    pub fn verify(&self) -> Result<()> {
        account::unchanged(&self.record).map(|_| ())
    }

    pub fn subscription(&mut self) -> Result<Value> {
        self.gateway("GET", "/v1/subscription", None, true)
    }
    pub fn rates(&mut self) -> Result<Value> {
        self.gateway("GET", "/v1/model-token-rates", None, false)
    }
    pub fn bonus(&mut self) -> Result<Value> {
        self.gateway(
            "POST",
            "/v1/subscription/bonus/claim",
            Some(&sdk::json!({})),
            true,
        )
    }

    fn gateway(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        version: bool,
    ) -> Result<Value> {
        self.verify()?;
        let project = self.credential.signing.project_id.clone();
        let result = match client::gateway_control(&self.credential, method, path, body, version) {
            Err(error) if error.code.as_deref() == Some(errors::CONTROL_EXPIRED) => {
                self.renew()?;
                if self.credential.signing.project_id.is_empty() {
                    self.credential.signing.project_id = project;
                }
                self.verify()?;
                client::gateway_control(&self.credential, method, path, body, version)
            }
            result => result,
        };
        self.verify()?;
        result
    }

    pub fn cloud(&mut self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        self.verify()?;
        let project = self.credential.signing.project_id.clone();
        let result = match client::cloud(&self.credential, method, path, body, false) {
            Err(error) if error.code.as_deref() == Some(errors::CONTROL_EXPIRED) => {
                self.renew()?;
                if self.credential.signing.project_id.is_empty() {
                    self.credential.signing.project_id = project;
                }
                self.verify()?;
                client::cloud(&self.credential, method, path, body, false)
            }
            result => result,
        };
        self.verify()?;
        result
    }

    pub fn resolve_project(&mut self) -> Result<()> {
        if !self.credential.signing.project_id.is_empty() {
            return Ok(());
        }
        self.verify()?;
        let project = match oauth_gateway::project(&self.credential) {
            Err(error) if error.code.as_deref() == Some(errors::CONTROL_EXPIRED) => {
                self.renew()?;
                oauth_gateway::project(&self.credential)?
            }
            result => result?,
        };
        self.verify()?;
        // 项目只读解析用于本次签名；原账号未登记该值时不伪装成已落库凭据。
        self.credential.signing.project_id = project;
        Ok(())
    }
}
