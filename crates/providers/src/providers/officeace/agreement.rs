use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use crate::errors::GatewayError;
use super::{control, credentials::OfficeAceCredential, upstream};

const STATEMENT: &str = "/declaration/officeace.html";

pub async fn confirm(
    ctx: &PluginCtx,
    credential: &mut OfficeAceCredential,
) -> Result<(), GatewayError> {
    let current = control::cloud_call(ctx, credential, "GET", "/v1/agreement?biz_type=officeclaw", None).await?;
    let rows = statements(&current)?;
    let pending: Vec<Value> = rows.iter().filter(|row| !accepted(row)).map(|row| json!({
        "privacy_statement": STATEMENT,
        "version": row["version"],
        "agree_declaration": 1,
    })).collect();
    if pending.is_empty() { return Ok(()); }
    let submitted = control::cloud_call(ctx, credential, "POST", "/v1/agreement", Some(json!({
        "agreement_list": pending,
    }))).await;
    if let Err(error) = &submitted {
        if error.code.as_deref() != Some(upstream::RESULT_UNCERTAIN) {
            return Err(error.clone());
        }
    }
    let checked = control::cloud_call(ctx, credential, "GET", "/v1/agreement?biz_type=officeclaw", None).await?;
    if statements(&checked)?.iter().all(|row| accepted(row)) {
        Ok(())
    } else {
        Err(GatewayError::with_status(409, "服务声明尚未确认生效，未提交套餐订单；请稍后查询或在官方客户端确认"))
    }
}

fn statements(value: &Value) -> Result<Vec<&Value>, GatewayError> {
    let rows = value.get("data").and_then(Value::as_array).ok_or_else(|| GatewayError::with_status(
        502, "官方未返回有效的服务声明清单，未自动确认协议",
    ))?;
    if rows.is_empty() || rows.iter().any(|row| {
        row.get("type").and_then(Value::as_str) != Some("officeclaw")
            || row.get("privacy_statement").and_then(Value::as_str) != Some(STATEMENT)
            || row.get("version").and_then(Value::as_str).is_none_or(|version| version.trim().is_empty())
    }) {
        return Err(GatewayError::with_status(409, "官方服务声明已变化，请在官方客户端查看并确认后再开通"));
    }
    Ok(rows.iter().collect())
}

fn accepted(row: &Value) -> bool {
    row.get("agree_flag").or_else(|| row.get("agree_declaration"))
        .is_some_and(|value| value.as_str() == Some("1") || value.as_i64() == Some(1))
}
