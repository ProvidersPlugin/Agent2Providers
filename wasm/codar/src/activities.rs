//! 只接入官方免费模型活动；查询不自动领取，领取状态不明时不再次提交。
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    Error, Result, Value,
};

use crate::{account, endpoints, network, token, util};

const ID: &str = "codar-free-model";
const KEY: &str = "codar-free-claim";

fn status(current: &account::Snapshot) -> Result<Value> {
    let value = current.request("GET", endpoints::API, endpoints::FREE, None, 8000)?;
    let data = network::data(&value);
    if !data.is_object() {
        return Err(Error::http(502, "科搭免费模型活动返回格式无效"));
    }
    Ok(data.clone())
}

fn campaign(data: &Value, confirming: bool) -> Value {
    let status = match data["status"].as_str() {
        Some("available") if confirming => "confirming",
        Some("available") => "claimable",
        Some("active" | "claimed") => "claimed",
        Some("expired" | "ended") => "expired",
        _ => "ineligible",
    };
    json!({ "campaignId": ID, "title": "科搭新用户免费模型", "titleEn": "Codar free-model activity",
        "description": "领取官方当前提供的免费模型活动；资格、支持模型和有效期以上游为准，不购买或续费。",
        "descriptionEn": "Claim the current official free-model activity. Eligibility and duration are decided upstream. No purchase or renewal.",
        "status": status, "startsAt": util::timestamp(&data["start_at"]), "endsAt": util::timestamp(&data["end_at"]),
        "expiresAt": util::timestamp(&data["free_until"]), "claimedAt": util::timestamp(&data["claimed_at"]),
        "amount": null, "unit": "免费模型", "validDays": data["valid_days"] })
}

pub fn preview() -> Result<Value> {
    token::ensure(false)?;
    let current = account::current()?;
    let data = status(&current)?;
    let old = state::get(Scope::Account, KEY, 1)?;
    if !old.compatible {
        return Err(Error::http(409, "科搭活动记录格式不兼容"));
    }
    let confirming =
        old.value["identity"] == current.identity && old.value["phase"] == "confirming";
    Ok(
        json!({ "campaigns": [campaign(&data, confirming)], "notice": "此查询不会自动领取活动。",
        "riskNote": "免费期结束后模型可能按积分计费，以官方活动说明为准。", "officialUrl": "https://codar.speedai.com" }),
    )
}

pub fn claim(id: &str) -> Result<Value> {
    if id != ID {
        return Ok(
            json!({ "campaignId": id, "outcome": "inactive", "message": "该科搭活动尚未接入领取" }),
        );
    }
    token::ensure(false)?;
    let _lock = sdk::AccountLock::acquire()?;
    let current = account::current()?;
    current.verify()?;
    let data = status(&current)?;
    let old = state::get(Scope::Account, KEY, 1)?;
    if !old.compatible {
        return Err(Error::http(409, "科搭活动记录格式不兼容，未提交领取"));
    }
    let active = matches!(data["status"].as_str(), Some("active" | "claimed"));
    if active {
        return Ok(
            json!({ "campaignId": ID, "outcome": "already_claimed", "message": "科搭免费模型活动已领取", "campaign": campaign(&data, false) }),
        );
    }
    if data["status"] != "available" {
        return Ok(
            json!({ "campaignId": ID, "outcome": "inactive", "message": "当前账号没有可领取的科搭免费模型活动", "campaign": campaign(&data, false) }),
        );
    }
    if old.value["identity"] == current.identity
        && matches!(old.value["phase"].as_str(), Some("confirming" | "claimed"))
    {
        return Ok(
            json!({ "campaignId": ID, "outcome": "confirming", "message": "上次活动领取结果未知，只回查，不重复提交", "campaign": campaign(&data, true) }),
        );
    }
    current.unchanged()?;
    if !state::compare_exchange(
        Scope::Account,
        KEY,
        1,
        old.revision,
        json!({ "identity": current.identity, "phase": "confirming", "at": util::now()? }),
        0,
    )? {
        return Err(Error::http(409, "科搭活动领取状态已变化"));
    }
    let submitted = current.request("POST", endpoints::API, endpoints::FREE_CLAIM, None, 8000);
    let after = status(&current);
    let confirmed = after
        .as_ref()
        .ok()
        .filter(|data| matches!(data["status"].as_str(), Some("active" | "claimed")));
    if let Some(data) = confirmed {
        let old = state::get(Scope::Account, KEY, 1)?;
        current.unchanged()?;
        if !old.compatible
            || !state::compare_exchange(
                Scope::Account,
                KEY,
                1,
                old.revision,
                json!({ "identity": current.identity, "phase": "claimed", "at": util::now()? }),
                0,
            )?
        {
            return Err(Error::http(
                409,
                "科搭活动已领取，但本地记录未能保存，请查询状态，勿重复提交",
            ));
        }
        return Ok(
            json!({ "campaignId": ID, "outcome": "claimed", "message": "科搭已确认免费模型活动领取成功", "campaign": campaign(data, false) }),
        );
    }
    if let Err(error) = submitted {
        if error
            .status
            .is_some_and(|status| (400..500).contains(&status))
        {
            let old = state::get(Scope::Account, KEY, 1)?;
            current.unchanged()?;
            if !old.compatible
                || !state::compare_exchange(
                    Scope::Account,
                    KEY,
                    1,
                    old.revision,
                    json!({ "identity": current.identity, "phase": "rejected" }),
                    0,
                )?
            {
                return Err(Error::http(409, "科搭活动记录已变化，请先查询状态"));
            }
            return Ok(
                json!({ "campaignId": ID, "outcome": "failed", "message": error.message, "campaign": null }),
            );
        }
    }
    Ok(
        json!({ "campaignId": ID, "outcome": "confirming", "message": "活动领取已提交但结果未确认，不自动重复领取", "campaign": null }),
    )
}
