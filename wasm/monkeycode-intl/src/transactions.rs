//! 一次只查指定页，不遍历历史。资金数值原样保留，单位待确认，不除以 1000。
//! amount_daily 属于钱包资金字段，不是 daily_token_*；仅显式 in/out 确认方向。

use crate::{
    budget::Budget, client, endpoints::ReadEndpoint, errors, redact::Guard, snapshot::Snapshot,
    util,
};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn read(account: &Snapshot, params: &Value, budget: &Budget) -> Result<Value> {
    if !params.is_null() && !params.is_object() {
        return Err(Error::http(400, "国际流水查询参数必须是对象"));
    }
    if params.as_object().is_some_and(|fields| {
        fields
            .keys()
            .any(|key| !["page", "size", "next_token"].contains(&key.as_str()))
    }) {
        return Err(Error::http(
            400,
            "国际流水仅接受 page、size、next_token 参数",
        ));
    }
    let page = util::page_number(params, "page", 1, 100_000)?;
    let size = util::page_number(params, "size", 20, 100)?;
    let mut query = vec![("page", page.to_string()), ("size", size.to_string())];
    if let Some(value) = params.get("next_token").filter(|value| !value.is_null()) {
        let token = value
            .as_str()
            .filter(|token| {
                !token.is_empty()
                    && token.len() <= 4096
                    && !token.chars().any(char::is_control)
                    && !account.guard.contains(token)
            })
            .ok_or_else(|| Error::http(400, "国际流水下一页标识无效"))?;
        query.push(("next_token", token.to_string()));
    }
    account.unchanged()?;
    let result = client::get(&account.session, ReadEndpoint::Transactions, &query, budget);
    account.unchanged()?;
    let data = result?.data;
    let rows = data["transactions"]
        .as_array()
        .ok_or_else(|| errors::protocol(ReadEndpoint::Transactions, "缺少 transactions 数组"))?;
    if rows.len() > 1000 || rows.iter().any(|row| !row.is_object()) {
        return Err(errors::protocol(
            ReadEndpoint::Transactions,
            "流水条目格式变化或数量超限",
        ));
    }
    let mut guard = account.guard.clone();
    // next_token 仅用于指定页继续读取，不当作 Session；其它凭据照常保护。
    guard.observe(&data)?;
    let mut warnings = vec![
        "流水为指定单页；国际资金单位待确认，未换算积分或货币。amount_daily 不是免费 Token"
            .to_string(),
    ];
    let transactions: Vec<Value> = rows
        .iter()
        .enumerate()
        .map(|(index, row)| project(row, &guard, page, index))
        .collect();
    if transactions
        .iter()
        .any(|row| row["amountRaw"].is_null() || row["direction"].is_null())
    {
        warnings.push("部分流水缺少有效金额或明确方向，保留未知；未按 kind 或余额差猜测".into());
    }
    let page_data = data.get("page").filter(|page| page.is_object());
    let has_next = page_data.and_then(|page| page["has_next_page"].as_bool());
    let next = page_data
        .and_then(|page| page["next_token"].as_str())
        .filter(|token| {
            !token.is_empty()
                && token.len() <= 4096
                && !token.chars().any(char::is_control)
                && !guard.contains(token)
        });
    if page_data.is_some_and(|page| !page["next_token"].is_null()) && next.is_none() {
        warnings.push("流水下一页标识无效或含受保护内容，未公开该值；分页状态待确认".into());
    }
    if has_next.is_none() {
        warnings.push("流水分页完成标志未知，未宣称完整历史".into());
    }
    let total = page_data
        .and_then(|page| page["total_count"].as_u64())
        .filter(|total| *total <= util::MAX_SAFE_INTEGER as u64);
    Ok(
        json!({ "transactions": transactions, "readOnly": true, "unit": "单位待确认", "unitKnown": false,
            "internationalVerified": false, "warnings": warnings,
            "page": { "number": page, "size": size, "has_next_page": has_next, "next_token": next, "total_count": total },
            "lastPage": has_next.map(|has_next| !has_next),
            "completeHistory": if has_next == Some(false) && page == 1 && query.len() == 2 { Some(true) } else { None }
        }),
    )
}

fn project(row: &Value, guard: &Guard, page: u32, index: usize) -> Value {
    let amount = util::number(row, "amount");
    let direction = match util::text(row, "inout_type") {
        Some("in") => Some("in"),
        Some("out") => Some("out"),
        _ => None,
    };
    let signed = amount.zip(direction).map(|(amount, direction)| {
        if direction == "out" {
            -amount.abs()
        } else {
            amount.abs()
        }
    });
    let created = util::integer(row, "created_at").filter(|created| *created >= 0);
    let created_ms = created
        .and_then(|created| created.checked_mul(1000))
        .filter(|created| *created <= util::MAX_SAFE_INTEGER);
    let kind = guard.field(row, "kind", 128);
    let remark = guard.field(row, "remark", 512);
    let name = remark
        .as_deref()
        .or(kind.as_deref())
        .unwrap_or("资金流水（单位待确认）");
    json!({ "key": format!("transaction:{page}:{index}"), "name": name,
        "record": true, "amount": signed, "amountRaw": amount,
        "amountBalanceRaw": util::number(row, "amount_balance"), "amountDailyRaw": util::number(row, "amount_daily"),
        "unit": "单位待确认", "unitKnown": false, "converted": false, "direction": direction,
        "kind": kind, "remark": remark, "created_at": created, "createdAt": created_ms, "at": created_ms,
        "internationalVerified": false })
}
