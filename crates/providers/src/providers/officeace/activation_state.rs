use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use crate::errors::GatewayError;
use super::{credentials::OfficeAceCredential, trial};

const LEDGER: &str = "officeaceTrialOrder";
const PENDING_WINDOW_MS: i64 = 5 * 60 * 1000;

pub async fn read(ctx: &PluginCtx, account: &str) -> Result<Value, GatewayError> {
    ctx.ledger_get(account, LEDGER).await.map_err(|error| GatewayError::with_status(
        502, format!("无法读取套餐开通记录，未提交订单：{error}"),
    ))
}

pub async fn write(ctx: &PluginCtx, account: &str, value: &Value) -> Result<(), GatewayError> {
    ctx.ledger_put(account, LEDGER, value).await.map_err(|error| GatewayError::with_status(
        502, format!("无法保存套餐开通记录，请先查询官方结果，勿重复提交：{error}"),
    ))
}

pub fn for_credential(value: &Value, credential: &OfficeAceCredential) -> bool {
    value.get("identity").and_then(Value::as_str) == Some(identity(credential).as_str())
}

pub fn identity(credential: &OfficeAceCredential) -> String {
    crate::flight::fingerprint(&format!("{}|{}", credential.base_url, credential.model_app_key))
}

pub fn submitted(credential: &OfficeAceCredential) -> Value {
    json!({
        "identity": identity(credential),
        "status": "submitting",
        "submittedAt": crate::logging::now_ms(),
        "sku": trial::SKU,
        "autoRenew": false,
    })
}

pub fn pending_view(ledger: &Value, resources: &[Value]) -> Option<Value> {
    if ledger.get("status").and_then(Value::as_str) == Some("active") {
        return Some(view("expired", "这条账号曾开通过体验套餐，当前已无有效套餐；这里不会自动续费或重新购买"));
    }
    if ledger.get("status").and_then(Value::as_str) == Some("rejected") {
        return None;
    }
    let submitted = ledger.get("submittedAt").and_then(Value::as_i64)?;
    let old = crate::logging::now_ms().saturating_sub(submitted) >= PENDING_WINDOW_MS;
    if trial::has_open_order(resources) && !old {
        return Some(view("pending", "官方存在未完成或正在生效的套餐订单，请稍后查询；不会重复下单"));
    }
    Some(if old {
        view("unknown", "开通已超过确认时间，但尚未查到生效套餐；为避免重复下单，请到官方客户端核对订单")
    } else {
        view("pending", "零元套餐订单已提交，正在等待官方生效，请稍后查询，不要重复提交")
    })
}

pub fn view(status: &str, message: &str) -> Value {
    json!({
        "status": status,
        "canActivate": status == "inactive",
        "message": message,
        "checkedAt": crate::logging::now_ms(),
    })
}

pub fn report(status: &str, message: &str) -> Value {
    json!({ "status": status, "message": message, "checkedAt": crate::logging::now_ms() })
}
