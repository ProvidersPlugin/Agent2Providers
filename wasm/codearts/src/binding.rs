//! 当前调用绑定账号、宿主冻结身份与完整凭据摘要。摘要只放账号私有状态，不公开。

use agent2api_wasm_sdk::{self as sdk, Result, Value};
use serde::{Deserialize, Serialize};

use crate::{credentials::Credential, errors, util};

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    provider: String,
    account: String,
    host_identity: String,
    declared_identity: String,
    credential_digest: String,
}

pub struct Account {
    pub credential: Credential,
    pub binding: Binding,
    pub needs_relogin: bool,
    raw_secret: Value,
}

fn needs_relogin(record: &Value) -> bool {
    record["needsRelogin"] == true
        || record["public"]["needsRelogin"] == true
        || record["secret"]["needsRelogin"] == true
}

fn validate_record(record: &Value, allow_disabled: bool) -> Result<()> {
    if record["provider"].as_str() != Some("codearts") || !record["secret"].is_object() {
        return Err(util::invalidated());
    }
    if !allow_disabled && sdk::account_disabled(record) {
        return Err(agent2api_wasm_sdk::Error::http(
            409,
            "CodeArts 账号已停用，已停止本次只读查询；未继续请求或保存新结果",
        )
        .with_code("codearts_account_disabled"));
    }
    Ok(())
}

impl Account {
    pub fn current() -> Result<Self> {
        Self::read(false)
    }

    pub fn current_refresh() -> Result<Self> {
        Self::read(true)
    }

    fn read(allow_disabled: bool) -> Result<Self> {
        let record = sdk::account().map_err(errors::host)?;
        validate_record(&record, allow_disabled)?;
        let id = record["id"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 512 && !id.contains('\0'))
            .ok_or_else(util::invalidated)?;
        let host_identity = record["identity"]
            .as_str()
            .filter(|identity| {
                !identity.is_empty() && identity.len() <= 512 && !identity.contains('\0')
            })
            .ok_or_else(util::invalidated)?;
        let raw_secret = record["secret"].clone();
        let credential = Credential::parse(&raw_secret)?;
        // 原样摘要包括导入代次、完整长期材料与身份，账号重新导入不会悄悄复用旧目录。
        let bytes = serde_json::to_vec(&raw_secret)
            .map_err(|_| errors::protocol("CodeArts 账号摘要无法计算"))?;
        let binding = Binding {
            provider: "codearts".into(),
            account: id.into(),
            host_identity: host_identity.into(),
            declared_identity: credential.identity(),
            credential_digest: util::digest(&[&bytes]),
        };
        // 旧原生失效标记可能在宿主公开视图，不随secret摊平；只认明确true，不从JWT猜测。
        let needs_relogin = needs_relogin(&record);
        Ok(Self {
            credential,
            binding,
            needs_relogin,
            raw_secret,
        })
    }

    pub fn unchanged(&self) -> Result<()> {
        self.check_unchanged(false)
    }

    pub fn unchanged_refresh(&self) -> Result<()> {
        self.check_unchanged(true)
    }

    fn check_unchanged(&self, allow_disabled: bool) -> Result<()> {
        let record = sdk::account().map_err(errors::host)?;
        validate_record(&record, allow_disabled)?;
        if record["id"].as_str() != Some(self.binding.account.as_str())
            || record["identity"].as_str() != Some(self.binding.host_identity.as_str())
            || needs_relogin(&record) != self.needs_relogin
            || record["secret"] != self.raw_secret
        {
            return Err(util::invalidated());
        }
        Ok(())
    }

    pub fn readable(&self) -> Result<()> {
        self.credential.require_readable(util::now()?)
    }
}

pub fn has_account(params: &Value) -> bool {
    params["account"]
        .as_str()
        .is_some_and(|account| !account.is_empty())
}
