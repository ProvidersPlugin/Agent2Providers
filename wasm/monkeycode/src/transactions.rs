//! 官方 Api.ts + wallet-dialog.tsx，固定 ad169c9b946ea04a1fc19a7abff9815d38116a73。
//! 一次只读取指定页，不自动遍历全部历史。amount 系列均为毫积分；daily 金额
//! 不是 daily_token_balance。缺少金额或收支方向不猜 0、不拿余额差推算。

use agent2api_wasm_sdk::{json, Error, Result, Value};

use crate::{account::Snapshot, client, endpoints::ReadEndpoint, errors, util};

pub fn read(account: &Snapshot, params: &Value) -> Result<Value> {
    let page = util::page_number(params, "page", 1, 100_000)?;
    let size = util::page_number(params, "size", 20, 100)?;
    let mut query = vec![("page", page.to_string()), ("size", size.to_string())];
    if let Some(raw) = params.get("next_token").filter(|raw| !raw.is_null()) {
        let token = raw
            .as_str()
            .filter(|token| {
                !token.is_empty()
                    && token.len() <= 4096
                    && !token.chars().any(char::is_control)
                    && !token.contains(&account.session)
            })
            .ok_or_else(|| Error::http(400, "MonkeyCode 流水下一页标识无效"))?;
        query.push(("next_token", token.to_string()));
    }
    account.unchanged()?;
    let result = client::get(&account.session, ReadEndpoint::Transactions, &query);
    account.unchanged()?;
    let data = result?;
    let rows = data["transactions"]
        .as_array()
        .ok_or_else(|| errors::protocol(ReadEndpoint::Transactions, "缺少 transactions 数组"))?;
    if rows.len() > 1000 || rows.iter().any(|row| !row.is_object()) {
        return Err(errors::protocol(
            ReadEndpoint::Transactions,
            "流水条目无效或超过大小限制",
        ));
    }
    let transactions: Vec<Value> = rows
        .iter()
        .enumerate()
        .map(|(index, row)| project(row, &account.session, page, index))
        .collect();
    let next = data["page"]["next_token"].as_str().filter(|value| {
        value.len() <= 4096
            && !value.chars().any(char::is_control)
            && !value.contains(&account.session)
    });
    let has_next = data["page"]["has_next_page"].as_bool();
    Ok(json!({
        "transactions": transactions,
        "page": { "number": page, "size": size, "has_next_page": has_next,
            "next_token": next, "total_count": data["page"]["total_count"].as_u64() },
        "lastPage": has_next.map(|has_next| !has_next),
        "completeHistory": page == 1 && query.len() == 2 && has_next == Some(false),
        "readOnly": true
    }))
}

fn direction(row: &Value) -> Option<i8> {
    match util::text(row, &["inout_type"]) {
        "in" => return Some(1),
        "out" => return Some(-1),
        _ => {}
    }
    match util::text(row, &["kind"]) {
        "signup_bonus" | "voucher_exchange" | "invitation_reward" | "pro_upgrade_refund"
        | "daily_grant" | "top_up" | "checkin" => Some(1),
        "vm_consumption"
        | "model_consumption"
        | "mcp_tool_consumption"
        | "pro_subscription"
        | "pro_auto_renew"
        | "ultra_subscription"
        | "ultra_auto_renew"
        | "violation_fine" => Some(-1),
        _ => None,
    }
}

fn project(row: &Value, session: &str, page: u32, index: usize) -> Value {
    let raw = util::number(row, "amount");
    let balance = util::number(row, "amount_balance");
    let daily = util::number(row, "amount_daily");
    // 只有两部分都存在才允许合计；真实 0 不能被 fallback 覆盖。
    let total = raw
        .or_else(|| balance.zip(daily).map(|(balance, daily)| balance + daily))
        .filter(|value| value.is_finite());
    let signed = total
        .zip(direction(row))
        .map(|(amount, sign)| amount.abs() * f64::from(sign) / 1000.0);
    let kind = util::public_text(util::text(row, &["kind"]), session, 128);
    let remark = util::public_text(util::text(row, &["remark"]), session, 512);
    let created = util::integer(row, "created_at").filter(|at| *at >= 0);
    let created_ms = created
        .and_then(|at| at.checked_mul(1000))
        .filter(|at| *at <= (1_i64 << 53) - 1);
    json!({
        "key": format!("transaction:{page}:{index}"),
        "name": if remark.is_empty() { if kind.is_empty() { "积分流水" } else { &kind } } else { &remark },
        "record": true,
        "unit": "积分",
        "amount": signed,
        "unsignedAmount": total.map(|amount| amount / 1000.0),
        "amountBalance": balance.map(|amount| amount / 1000.0),
        "amountDaily": daily.map(|amount| amount / 1000.0),
        "amountRaw": raw,
        "amountBalanceRaw": balance,
        "amountDailyRaw": daily,
        "rawUnit": "毫积分",
        "direction": direction(row).map(|sign| if sign > 0 { "in" } else { "out" }),
        "kind": kind,
        "remark": remark,
        "created_at": created,
        "createdAt": created_ms
    })
}
