//! 只确认旧流程明确指定的服务声明；可选数据改进授权不在范围，未知声明必须人工处理。

use super::{control::Context, errors};
use agent2api_wasm_sdk::{json, Error, Result, Value};

const PATH: &str = "/declaration/officeace.html";
const QUERY: &str = "/v1/agreement?biz_type=officeclaw";

pub fn confirm(context: &mut Context) -> Result<()> {
    let value = context.cloud("GET", QUERY, None)?;
    let pending: Vec<Value> = statements(&value)?.into_iter().filter(|row| !accepted(row)).map(|row|
        json!({ "privacy_statement": PATH, "version": row["version"], "agree_declaration": 1 })).collect();
    if pending.is_empty() {
        return Ok(());
    }
    match context.cloud(
        "POST",
        "/v1/agreement",
        Some(&json!({ "agreement_list": pending })),
    ) {
        Err(error) if error.code.as_deref() != Some(errors::UNCERTAIN) => return Err(error),
        _ => {}
    }
    let checked = context.cloud("GET", QUERY, None)?;
    if statements(&checked)?.iter().all(|row| accepted(row)) {
        Ok(())
    } else {
        Err(Error::http(
            409,
            "服务声明尚未确认生效，未提交套餐订单；请稍后查询或在官方客户端确认",
        ))
    }
}

fn statements(value: &Value) -> Result<Vec<&Value>> {
    let rows = value["data"]
        .as_array()
        .ok_or_else(|| Error::http(502, "官方服务声明清单无效，未自动确认协议"))?;
    if rows.is_empty()
        || rows.iter().any(|row| {
            row["type"] != "officeclaw"
                || row["privacy_statement"] != PATH
                || row["version"]
                    .as_str()
                    .is_none_or(|value| value.trim().is_empty())
        })
    {
        return Err(Error::http(
            409,
            "官方服务声明已变化，请在官方客户端查看并确认后再开通",
        ));
    }
    Ok(rows.iter().collect())
}
fn accepted(row: &Value) -> bool {
    row.get("agree_flag")
        .or_else(|| row.get("agree_declaration"))
        .is_some_and(|value| value.as_str() == Some("1") || value.as_i64() == Some(1))
}
