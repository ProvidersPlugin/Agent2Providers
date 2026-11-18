//! 签到按上游滚动24小时窗口；提交前持久标记，结果不明只回查，不重复领取。
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope, Snapshot},
    Error, Result, Value,
};

use crate::{account, endpoints, errors, network, token, util};

const KEY: &str = "codar-sign";
const DAY: i64 = 86_400_000;

pub struct Status {
    pub signed: bool,
    pub until: Option<i64>,
}
impl Status {
    pub fn query(current: &account::Snapshot) -> Result<Self> {
        let reply = current.request(
            "POST",
            endpoints::ACCOUNT,
            endpoints::SIGN_STATUS,
            Some(&json!({ "username": current.secret["username"] })),
            5000,
        )?;
        let data = network::data(&reply);
        let signed = data
            .as_bool()
            .or_else(|| {
                ["isSigned", "signed", "hasSigned", "has_signed"]
                    .iter()
                    .find_map(|key| data[*key].as_bool())
            })
            .ok_or_else(|| Error::http(502, "科搭没有返回明确签到状态，未提交领取"))?;
        let wait = util::amount(&data["retry_after"])
            .filter(|seconds| *seconds > 0.0 && *seconds <= 7.0 * 86400.0);
        Ok(Self {
            signed,
            until: wait.map(|seconds| sdk::now().saturating_add((seconds * 1000.0).ceil() as i64)),
        })
    }
}

fn load() -> Result<Snapshot> {
    let old = state::get(Scope::Account, KEY, 1)?;
    if !old.compatible {
        return Err(Error::http(409, "科搭签到记录版本不兼容，未提交签到"));
    }
    Ok(old)
}
fn save(current: &account::Snapshot, old: &Snapshot, value: Value) -> Result<()> {
    current.unchanged()?;
    if !state::compare_exchange(Scope::Account, KEY, 1, old.revision, value, 0)? {
        return Err(Error::http(409, "科搭签到记录已变化，未重复领取"));
    }
    Ok(())
}
fn report(outcome: &str, message: &str, credit: Option<String>, until: Option<i64>) -> Value {
    json!({ "outcome": outcome, "message": message, "credit": credit, "streak": null, "until": until })
}
fn record(current: &account::Snapshot, phase: &str, until: Option<i64>) -> Value {
    json!({ "identity": current.identity, "phase": phase, "at": sdk::now(), "until": until })
}

pub fn claim() -> Result<Value> {
    token::ensure(false)?;
    let _lock = sdk::AccountLock::acquire()?;
    let current = account::current()?;
    current.verify()?;
    let before = Status::query(&current)?;
    let old = load()?;
    if before.signed {
        let until = before
            .until
            .or(Some(util::now()?.saturating_add(1_800_000)));
        save(&current, &old, record(&current, "claimed", until))?;
        return Ok(report(
            "done",
            "官方确认本轮签到已领取，没有重复提交",
            None,
            until,
        ));
    }
    if old.value["identity"] == current.identity
        && old.value["phase"] == "claimed"
        && old.value["until"]
            .as_i64()
            .is_some_and(|until| until > sdk::now())
    {
        return Ok(report(
            "done",
            "本轮科搭签到已有明确成功记录，未重复领取",
            None,
            old.value["until"].as_i64(),
        ));
    }
    let age = sdk::now().saturating_sub(old.value["at"].as_i64().unwrap_or(0));
    if old.value["identity"] == current.identity
        && old.value["phase"] == "confirming"
        && (0..DAY).contains(&age)
    {
        return Ok(report(
            "inactive",
            "上次签到结果未确认，本次只查询状态，不重复领取",
            None,
            Some(sdk::now().saturating_add(1_800_000)),
        ));
    }
    save(
        &current,
        &old,
        record(&current, "confirming", Some(sdk::now().saturating_add(DAY))),
    )?;
    let request_token = current.checked_access()?;
    let raw = network::request(
        "POST",
        endpoints::ACCOUNT,
        endpoints::SIGN,
        Some(&request_token),
        Some(&json!({ "username": current.secret["username"] })),
        8000,
    );
    current.unchanged()?;
    if current.secret["source"] == "desktop" && current.access()? != request_token {
        return Err(crate::errors::changed());
    }
    let after = Status::query(&current);
    if let Ok(status) = &after {
        if status.signed {
            let until = status.until.or(Some(sdk::now().saturating_add(DAY)));
            save(&current, &load()?, record(&current, "claimed", until))?;
            let credit = raw
                .as_ref()
                .ok()
                .filter(|reply| (200..300).contains(&reply.status) && !errors::failed(&reply.body))
                .and_then(|reply| util::amount(&network::data(&reply.body)["granted"]))
                .filter(|value| *value > 0.0)
                .map(util::display);
            return Ok(report("claimed", "科搭已确认签到积分发放", credit, until));
        }
    }
    match raw {
        Ok(reply) => {
            let code = util::number(&reply.body["code"]);
            let words = errors::message(&reply.body).to_ascii_lowercase();
            let already = regex::Regex::new(r"24\s*小时|仅可领取一次|once\s+(?:every|in|per)\s+(?:a\s+)?(?:24\s*(?:hours?|h)|day)|already\s+(?:claimed|signed|checked)")
                .map(|pattern| pattern.is_match(&words)).unwrap_or(false);
            if reply.status < 400 && matches!(code, None | Some(200.0) | Some(400.0)) && already {
                let until = util::amount(&network::data(&reply.body)["retry_after"])
                    .filter(|seconds| *seconds > 0.0 && *seconds <= 7.0 * 86400.0)
                    .map(|seconds| sdk::now().saturating_add((seconds * 1000.0) as i64))
                    .or(Some(sdk::now().saturating_add(1_800_000)));
                save(&current, &load()?, record(&current, "claimed", until))?;
                return Ok(report(
                    "done",
                    "科搭确认24小时内已领过，本次未重复到账",
                    None,
                    until,
                ));
            }
            match reply.checked(&[current.access()?]) {
                Ok(body)
                    if util::number(&body["code"]) == Some(200.0) || body["success"] == true =>
                {
                    let until = Some(sdk::now().saturating_add(DAY));
                    save(&current, &load()?, record(&current, "claimed", until))?;
                    Ok(report(
                        "claimed",
                        "科搭返回明确的签到成功回执，状态回查暂未更新",
                        util::amount(&network::data(&body)["granted"]).map(util::display),
                        until,
                    ))
                }
                Err(error)
                    if error
                        .status
                        .is_some_and(|status| (400..500).contains(&status)) =>
                {
                    save(&current, &load()?, record(&current, "rejected", None))?;
                    Ok(report("failed", &error.message, None, None))
                }
                _ => Ok(report(
                    "inactive",
                    "签到提交结果未确认，只保留记录并等待回查，不重复领取",
                    None,
                    None,
                )),
            }
        }
        Err(_) => Ok(report(
            "inactive",
            "签到请求已发出但结果未知，请稍后查询；不会自动重复领取",
            None,
            None,
        )),
    }
}

pub fn info() -> Result<Value> {
    token::ensure(false)?;
    let current = account::current()?;
    let status = Status::query(&current)?;
    Ok(json!({ "signed": status.signed, "until": status.until }))
}
