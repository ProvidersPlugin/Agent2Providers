//! 冻结 provider、record.id、宿主 identity、归一用户 ID 和完整 secret 摘要。
//! 宿主 identity 可为旧哈希主键，不要求 monkeycode- 前缀；不自行迁移/改写记录 ID。

use crate::{
    credentials::{self, Input},
    endpoints, provenance,
    redact::Guard,
    util,
};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub struct Snapshot {
    pub record: Value,
    pub id: String,
    pub host_identity: String,
    pub user_id: String,
    pub session: String,
    pub credential_hash: String,
    pub guard: Guard,
}

fn host_string(record: &Value, key: &str) -> Result<String> {
    let text = record
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| {
            !value.trim().is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
        })
        .ok_or_else(|| Error::http(403, "MonkeyCode 国际主程序账号快照不完整，未猜测身份"))?;
    Ok(text.to_string())
}

impl Snapshot {
    fn from(record: Value) -> Result<Self> {
        if record["provider"] != endpoints::ID || sdk::account_disabled(&record) {
            return Err(Error::http(403, "当前账号不属于国际包或已停用，未发送请求")
                .with_code("monkeycode_intl_account_changed"));
        }
        provenance::check(&record)?;
        if record["secret"].is_null() {
            return Err(Error::http(401, "国际账号缺少凭据，请重新导入"));
        }
        let id = host_string(&record, "id")?;
        let host_identity = host_string(&record, "identity")?;
        let input: Input = credentials::parse(&record["secret"])?;
        let user_id = input
            .user_id
            .ok_or_else(|| Error::http(401, "国际账号缺少有效用户 UUID，请重新导入"))?;
        let credential_hash = util::fingerprint(&record["secret"].to_string());
        Ok(Self {
            record,
            id,
            host_identity,
            user_id,
            session: input.session,
            credential_hash,
            guard: input.guard,
        })
    }

    pub fn binding(&self) -> String {
        util::fingerprint(
            &json!({ "provider": endpoints::ID, "recordId": self.id,
            "hostIdentity": self.host_identity, "userId": self.user_id,
            "credentials": self.credential_hash })
            .to_string(),
        )
    }

    pub fn unchanged(&self) -> Result<()> {
        let next = sdk::account().map_err(|_| util::changed())?;
        if next["provider"] != self.record["provider"]
            || next["id"] != self.record["id"]
            || next["identity"] != self.record["identity"]
            || next["secret"] != self.record["secret"]
            || sdk::account_disabled(&next)
        {
            return Err(util::changed());
        }
        let next = Self::from(next).map_err(|_| util::changed())?;
        if next.user_id != self.user_id || next.credential_hash != self.credential_hash {
            return Err(util::changed());
        }
        Ok(())
    }
}

pub fn current() -> Result<Snapshot> {
    Snapshot::from(sdk::account()?)
}

pub fn selected(params: &Value) -> Result<Option<Snapshot>> {
    match params.get("account") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.is_empty() => Ok(None),
        Some(Value::String(value)) => {
            let account = current()?;
            if account.id != *value {
                return Err(util::changed());
            }
            Ok(Some(account))
        }
        _ => Err(Error::http(400, "国际账号选择无效")),
    }
}
