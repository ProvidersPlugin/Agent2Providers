//! 只读 delivery；不调用领取、确认，不读写 codeartsWelfare/onboardingSettled 台账。
//! CONFIRMED/CONSUMED 没有日期，只表示本次观察到已确认，不能生成“今日已领”。

use agent2api_wasm_sdk::{json, Result, Value};

use crate::{binding::Account, endpoints::ReadRequest, errors, gates, network, util};

pub fn preview(newbie_only: bool) -> Result<Value> {
    let account = Account::current()?;
    let payload = network::get(&account, ReadRequest::Delivery, 1024 * 1024)?;
    if payload["code"].as_i64() != Some(0) {
        return Err(errors::protocol(
            "CodeArts 活动状态未得到 code=0 确认，未当作没有活动",
        ));
    }
    let items = payload["data"]["items"]
        .as_array()
        .filter(|items| items.len() <= 256)
        .ok_or_else(|| errors::protocol("CodeArts 活动状态缺少有效 items 数组"))?;
    let secrets = account.credential.secrets();
    let mut rows = Vec::new();
    for item in items {
        if !item.is_object() {
            return Err(errors::protocol("CodeArts 活动条目不是对象"));
        }
        let kind = errors::upstream_text(item, "type", &secrets, 80)?;
        if newbie_only && kind != "NEW_USER_REGISTER" {
            continue;
        }
        let id = match item.get("campaignId") {
            Some(Value::String(id))
                if !id.is_empty()
                    && id.len() <= 256
                    && !id.chars().any(char::is_control)
                    && !errors::contains_secret(id, &secrets)
                    && !id.contains("://") =>
            {
                json!(id)
            }
            Some(Value::Number(id)) if id.as_i64().is_some() || id.as_u64().is_some() => json!(id),
            _ => return Err(errors::protocol("CodeArts 活动缺少可确认的标识")),
        };
        let title = errors::upstream_text(item, "title", &secrets, 256)?;
        let status = errors::upstream_text(item, "status", &secrets, 64)?;
        let unit = errors::upstream_text(item, "benefitUnit", &secrets, 32)?;
        let claimable = match item.get("claimable").filter(|value| !value.is_null()) {
            Some(value) => Some(
                value
                    .as_bool()
                    .ok_or_else(|| errors::protocol("CodeArts 活动资格字段不是布尔值"))?,
            ),
            None => None,
        };
        let amount = match item.get("benefitAmount").filter(|value| !value.is_null()) {
            Some(value) => Some(
                util::finite(Some(value))
                    .filter(|amount| *amount >= 0.0)
                    .ok_or_else(|| errors::protocol("CodeArts 活动金额不是有效数值"))?,
            ),
            None => None,
        };
        let confirmed =
            claimable == Some(false) && matches!(status.as_str(), "CONFIRMED" | "CONSUMED");
        let known = (kind == "USER_LOGIN" || kind == "NEW_USER_REGISTER") && unit == "CREDIT";
        let shown_title = display_title(&kind).map(str::to_string).unwrap_or_else(|| {
            if title.is_empty() {
                kind.clone()
            } else {
                title.clone()
            }
        });
        rows.push(json!({ "id": id, "kind": kind,
            "title": shown_title,
            "status": status, "upstreamClaimable": claimable, "benefitAmount": amount, "benefitUnit": unit,
            "observedConfirmed": confirmed, "claimedToday": null, "confirmedAt": null,
            "knownRewardType": known, "claimEnabled": known,
            "freeConfirmed": null, "agreementRequired": null, "paymentRequired": null }));
    }
    account.unchanged()?;
    Ok(
        json!({ "provider": "codearts", "readOnly": true, "observedAt": util::now()?, "campaigns": rows,
        "claimedToday": null, "claimedPoints": null, "claimEnabled": false,
        "message": "此预览只查询官方活动；已接通的每日积分及新手礼从各自领取入口执行",
        "note": "活动状态不是今日到账证明；套餐赠送积分不增加福利模型 token 池；本次未领取、确认、同意协议或付费",
        "httpBudget": gates::HTTP_BUDGET }),
    )
}

fn display_title(kind: &str) -> Option<&'static str> {
    match kind {
        "USER_LOGIN" => Some("用户登录送积分"),
        "DAILY_CLAIM" => Some("每日签到"),
        "NEW_USER_REGISTER" => Some("新用户注册礼"),
        "STUDENT_CERTIFIED" => Some("学生认证"),
        "INVITE_USER" => Some("邀请好友"),
        _ => None,
    }
}
