//! 保留旧 officeaceTrialOrder 提交台账与 SHA256 前16字节指纹；提交前必须可靠落盘。

use super::{credentials::Credentials, ledger, trial, util};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub fn read() -> Result<Value> {
    ledger::get(ledger::TRIAL).map_err(|_| Error::http(502, "无法读取套餐开通记录，未提交订单"))
}
pub fn write(value: &Value) -> Result<()> {
    ledger::put(ledger::TRIAL, value)
        .map_err(|_| Error::http(502, "无法保存套餐开通记录，请先查询官方结果，勿重复提交"))
}
pub fn binding(credential: &Credentials) -> String {
    util::fingerprint(&format!(
        "{}|{}",
        credential.base_url, credential.model_app_key
    ))
}
pub fn owns(value: &Value, credential: &Credentials) -> bool {
    value["identity"] == binding(credential)
}

pub fn submitted(credential: &Credentials) -> Value {
    json!({ "identity": binding(credential), "status": "submitting", "submittedAt": sdk::now(), "sku": trial::SKU, "autoRenew": false })
}
pub fn view(status: &str, message: &str) -> Value {
    json!({ "status": status, "canActivate": status == "inactive", "message": message, "checkedAt": sdk::now() })
}
pub fn report(status: &str, message: &str) -> Value {
    json!({ "status": status, "message": message, "checkedAt": sdk::now() })
}

pub fn pending(ledger: &Value, rows: &[Value]) -> Option<Value> {
    if ledger["status"] == "active" {
        return Some(view(
            "expired",
            "这条账号曾开通过体验套餐，当前已无有效套餐；这里不会自动续费或重新购买",
        ));
    }
    if ledger["status"] == "rejected" {
        return None;
    }
    let at = ledger["submittedAt"].as_i64()?;
    let old = sdk::now().saturating_sub(at) >= 5 * 60 * 1000;
    Some(if trial::has_order(rows) && !old {
        view(
            "pending",
            "官方存在未完成或正在生效的套餐订单，请稍后查询；不会重复下单",
        )
    } else if old {
        view(
            "unknown",
            "开通超过确认时间但尚未查到生效套餐；为避免重复下单，请到官方客户端核对",
        )
    } else {
        view(
            "pending",
            "零元套餐订单已提交，正在等待官方生效，请勿重复提交",
        )
    })
}
