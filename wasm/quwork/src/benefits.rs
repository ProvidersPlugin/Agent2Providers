//! 活动预览只读。兑换只处理用户确认后进入的 benefits.redeem/{code}，不自动购买。
//! 月度赠送只展示；兑换码需原设备标识，不能给普通账号暗读本机补齐。

use super::{account, checkin, client, endpoints, ledger, refresh};
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

fn daily(status: &checkin::Status, saved: &str) -> Value {
    json!({ "campaignId": format!("daily:{}", status.reset), "title": "QuWork 每日奖励", "titleEn": "QuWork daily reward",
        "description": "与每日签到共用领取状态，不会重复领取。", "descriptionEn": "Shares its claim state with daily check-in; rewards are never claimed twice.",
        "amount": status.points, "unit": "积分", "status": if status.claimed || saved == "claimed" { "claimed" }
            else if saved == "confirming" { "confirming" } else if status.can_claim { "claimable" } else { "ineligible" },
        "startsAt": null, "endsAt": status.reset, "claimedAt": null, "expiresAt": null, "validDays": null })
}

fn monthly(data: &Value) -> Result<Vec<Value>> {
    if !data.is_object() {
        return Err(Error::http(502, "QuWork 月度赠送记录格式异常"));
    }
    let Some(list) = data.get("list").filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let orders = list
        .as_array()
        .filter(|orders| orders.len() <= 100)
        .ok_or_else(|| Error::http(502, "QuWork 月度赠送记录格式异常"))?;
    let mut rows = Vec::new();
    for order in orders {
        let id = order["order_id"]
            .as_str()
            .map(str::to_string)
            .or_else(|| order["order_id"].as_u64().map(|id| id.to_string()))
            .unwrap_or_default();
        // 名字与稳定标识有长度上限，不能让异常上游字段撑满管理响应。
        if id.len() > 256 || id.chars().any(char::is_control) {
            return Err(Error::http(502, "QuWork 月度赠送订单标识格式异常"));
        }
        for (index, period) in order["periods"]
            .as_array()
            .into_iter()
            .flatten()
            .take(120)
            .enumerate()
        {
            let status = client::number(&period["gift_status"]).map(|value| value as i64);
            rows.push(json!({ "id": format!("{id}:{index}"), "orderId": id, "period": index + 1,
                "amount": client::number(&period["points"]).or_else(|| client::number(&period["quota"])),
                "scheduledAt": client::timestamp(&period["expected_gift_date"]), "grantedAt": client::timestamp(&period["actual_gift_date"]),
                "status": match status { Some(1) => "pending", Some(2) => "granted", Some(3) => "failed", Some(4) => "cancelled", Some(5) => "processing", _ => "unknown" } }));
        }
    }
    Ok(rows)
}

pub fn preview() -> Result<Value> {
    refresh::ensure_account()?;
    let record = sdk::account()?;
    let mut notices = Vec::new();
    let campaigns = match checkin::status() {
        Ok(status) => vec![daily(&status, &checkin::saved(&status)?)],
        Err(error) if error.code.as_deref() == Some("identity_changed") => return Err(error),
        Err(error) => {
            notices.push(error.message);
            Vec::new()
        }
    };
    let gifts = refresh::protected("GET", endpoints::MONTHLY, None)
        .and_then(|reply| reply.data("月度赠送查询"))
        .and_then(|data| monthly(&data));
    let gifts = match gifts {
        Ok(rows) => Some(rows),
        Err(error) if error.code.as_deref() == Some("identity_changed") => return Err(error),
        Err(error) => {
            notices.push(error.message);
            None
        }
    };
    account::unchanged(&record)?;
    Ok(
        json!({ "campaigns": campaigns, "riskNote": null, "monthlyGifts": gifts,
        "notice": notices.join("；"), "redemption": { "supported": true } }),
    )
}

pub fn claim(id: &str) -> Result<Value> {
    let before = checkin::status()?;
    if id != format!("daily:{}", before.reset) {
        return Ok(json!({ "campaignId": id, "outcome": "inactive", "campaign": null }));
    }
    let report = checkin::run_for(Some(before.reset))?;
    let outcome = match report["outcome"].as_str() {
        Some("claimed") => "claimed",
        Some("done") => "already_claimed",
        Some("ineligible") => "ineligible",
        Some("inactive") => "inactive",
        _ => "failed",
    };
    let latest = match checkin::status() {
        Ok(status) => status,
        Err(error) if error.code.as_deref() == Some("identity_changed") => return Err(error),
        Err(_) => before,
    };
    let saved = checkin::saved(&latest)?;
    Ok(
        json!({ "campaignId": id, "outcome": if saved == "confirming" && !latest.claimed { "confirming" } else { outcome },
        "campaign": daily(&latest, &saved), "message": report["message"] }),
    )
}

pub fn redeem(payload: &Value) -> Result<Value> {
    // 已有跨端UI进行二次确认，专用管理入口只传{code}；不能新增不兼容的必填字段。
    if payload["confirmed"] == false {
        return Err(Error::http(400, "请确认将权益兑换到当前账号"));
    }
    let code = account::text(payload, &["code"]);
    if code.is_empty() || code.len() > 32 || code.chars().any(char::is_control) {
        return Err(Error::http(400, "请填写 QuWork 权益兑换码或16位订单号"));
    }
    refresh::ensure_account()?;
    let _lock = AccountLock::acquire()?;
    let record = sdk::account()?;
    let secret = refresh::snapshot()?;
    account::unchanged(&record)?;
    let access = account::text(&secret, &["accessToken"]);
    if access.is_empty() {
        return Err(super::errors::missing_account());
    }
    let order = code.len() == 16 && code.bytes().all(|byte| byte.is_ascii_digit());
    let mut device = account::text(&secret, &["deviceIdentifier"]);
    if !order && device.is_empty() {
        let source=sdk::native_call("desktop.device.quwork.v1","read",json!({}))?;
        device=account::text(&source,&["deviceIdentifier"]);
    }
    if !order && !account::valid_device(&device) {
        return Err(Error::http(400, "QuWork 兑换码激活需要原客户端的64位设备标识；请重新导入含 deviceIdentifier 的凭证，或到官方客户端激活"));
    }
    let key = format!("quworkRedemption:{}", account::fingerprint(&code));
    let previous = ledger::get(&key)?;
    match previous.value["state"].as_str() {
        Some("succeeded") => return Ok(json!({ "outcome": "already_claimed" })),
        Some("confirming") => return Ok(json!({ "outcome": "confirming" })),
        _ => {}
    }
    ledger::save(
        &key,
        &previous,
        json!({ "state": "confirming", "at": sdk::now() }),
    )?;
    let body = if order {
        json!({ "order_id": code })
    } else {
        json!({ "invite_code": code, "device_identifier": device, "device_uuid": account::text(&secret, &["deviceUuid"]) })
    };
    account::unchanged(&record)?;
    let response = client::request(
        "POST",
        if order {
            endpoints::REDEEM
        } else {
            endpoints::ACTIVATE
        },
        &access,
        Some(&body),
    );
    account::unchanged(&record)?;
    let (state, outcome, message) = match response {
        Ok(reply) if reply.ok() => ("succeeded", "claimed", "QuWork 权益兑换成功"),
        Ok(reply) if reply.rejected() => (
            "rejected",
            "failed",
            "QuWork 未接受本次兑换，请核对权益或前往官方客户端确认",
        ),
        _ => (
            "confirming",
            "confirming",
            "兑换结果待确认，请到官方客户端核对；不会重复提交",
        ),
    };
    ledger::remember(&key, json!({ "state": state, "at": sdk::now() }))?;
    Ok(json!({ "outcome": outcome, "message": message }))
}
