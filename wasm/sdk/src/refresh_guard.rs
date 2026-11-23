use sha2::{Digest, Sha256};

use crate::{json, state::{self, Scope, Snapshot}, Error, Result, Value};

pub struct Reservation { key: String, stamp: String, revision: u64 }

fn stamp(material: &str) -> String { format!("{:x}", Sha256::digest(material.as_bytes())) }
fn read(key: &str) -> Result<Snapshot> {
    let snapshot = state::get(Scope::Account, key, 1)?;
    if !snapshot.compatible { return Err(unconfirmed()); }
    Ok(snapshot)
}
fn unconfirmed() -> Error {
    Error::http(409, "上次续期结果尚未确认，不会重复消耗原刷新凭据；请重新登录并更新凭据")
        .with_code("refresh_result_unconfirmed")
}

pub fn available(key: &str, material: &str) -> Result<bool> {
    Ok(read(key)?.value["stamp"] != stamp(material))
}

impl Reservation {
    pub fn acquire(key: &str, material: &str) -> Result<Self> {
        if material.is_empty() { return Err(unconfirmed()); }
        let snapshot = read(key)?;
        let stamp = stamp(material);
        if snapshot.value["stamp"] == stamp { return Err(unconfirmed()); }
        if !state::compare_exchange(Scope::Account, key, 1, snapshot.revision,
            json!({ "stamp": stamp }), 0)? { return Err(unconfirmed()); }
        let saved = read(key)?;
        if saved.value["stamp"] != stamp { return Err(unconfirmed()); }
        Ok(Self { key: key.into(), stamp, revision: saved.revision })
    }

    pub fn complete(self) -> Result<()> {
        if read(&self.key)?.value["stamp"] != self.stamp
            || !state::compare_exchange(Scope::Account, &self.key, 1, self.revision, Value::Null, 0)? {
            return Err(unconfirmed());
        }
        Ok(())
    }
}
