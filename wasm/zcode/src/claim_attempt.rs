//! 领取的逐套餐并发保护、回执与冷却。网络不确定不当成功，也不在同一轮重发。
use super::{
    account, claim_sources,
    claim_state::{Ledger, Rejection, Saved, VerificationRequired},
    claim_types::{Outcome, Plan},
    config,
    network::{self, integer, ZCODE},
    verification,
};
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

pub struct Attempt {
    pub outcome: Outcome,
    pub view: Value,
}

fn projected(outcome: Outcome, ledger: &Ledger, marketing_error: Option<String>) -> Attempt {
    let mut view = outcome.view();
    view["claimedAt"] = json!(ledger
        .plans
        .get(&outcome.plan_id)
        .and_then(Saved::claimed_at));
    view["dailyCheckin"] = ledger.daily();
    view["marketingError"] = json!(marketing_error);
    Attempt { outcome, view }
}

fn existing(ledger: &Ledger, plan: &Plan, version: &str) -> Option<Outcome> {
    let saved = ledger.plans.get(&plan.plan_id)?;
    if saved.claimed_for(plan) {
        return Some(Outcome::failed(
            plan,
            1003,
            "该套餐已领取，不重复提交",
            saved.claim_ends_at.or(plan.ends_at),
        ));
    }
    if let Some(retry) = saved
        .retry
        .as_ref()
        .filter(|retry| retry.retry_at.is_some_and(|at| at > sdk::now()))
    {
        // 验证失败允许用户提交新凭据；其它冷却保持，不把每次点击变成上游重复请求。
        if retry.failure != "captcha"
            && (retry.failure != "ineligible"
                || saved
                    .rejection
                    .as_ref()
                    .is_none_or(|reject| reject.version == version))
        {
            return Some(retry.clone());
        }
    }
    if saved.submitted_at.is_some() {
        let newer = saved.submitted_plan.as_ref().is_some_and(|before| {
            match (before.ends_at, plan.starts_at) {
                (Some(end), Some(start)) => start >= end,
                _ => before
                    .starts_at
                    .zip(plan.starts_at)
                    .is_some_and(|(before, next)| next > before),
            }
        });
        if !newer {
            return Some(Outcome::failed(
                plan,
                502,
                "该套餐上次提交结果未确认，未重复领取；请先查询官方套餐状态",
                None,
            ));
        }
    }
    if saved
        .in_flight_at
        .is_some_and(|at| sdk::now().saturating_sub(at) < 600_000)
    {
        let mut outcome = Outcome::failed(
            plan,
            1002,
            "该套餐已有领取在处理或等待核对，请稍后查询",
            None,
        );
        outcome.retry_at = saved.in_flight_at.map(|at| at + 600_000);
        return Some(outcome);
    }
    None
}

pub fn execute(
    record: &Value,
    plan: &Plan,
    supplied: Option<&Value>,
    deadline: i64,
) -> Result<Attempt> {
    let version = config::app_version()?;
    let claim_id = network::uuid()?;
    {
        let _lock = AccountLock::acquire()?;
        let current = account::record()?;
        if current["secret"] != record["secret"] {
            return Err(Error::http(409, "账号已变化，未发送旧账号的领取请求"));
        }
        let mut ledger = Ledger::load(record)?;
        if let Some(outcome) = existing(&ledger, plan, &version) {
            return finish_existing(record, plan, outcome, &mut ledger, &version);
        }
        let saved = ledger
            .plans
            .entry(plan.plan_id.clone())
            .or_insert_with(|| Saved::new(plan));
        if !saved.claimed_for(plan) {
            saved.claimed_at = None;
            saved.claim_starts_at = None;
            saved.claim_ends_at = None;
            saved.owned = None;
        }
        let mut fresh = plan.clone();
        fresh.merge(saved.plan.clone());
        saved.plan = fresh;
        saved.in_flight_at = Some(sdk::now());
        saved.in_flight_id = Some(claim_id.clone());
        // 先把“在途”可靠存下才请求上游；宿主取消/崩溃留下短冷却，不会立即再领一次。
        ledger.save()?;
    }
    let verification = match supplied {
        Some(payload) if payload["planId"] != plan.plan_id => {
            Err(Error::http(400, "验证票据与当前领取套餐不匹配"))
        }
        Some(payload) => verification::supplied(payload, record),
        None => verification::prepare(&plan.plan_id, &plan.name, deadline),
    };
    let proof = match verification {
        Ok(proof) => proof,
        Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
        Err(error) => {
            let code = if error.status == Some(428) {
                428
            } else {
                error.status.unwrap_or(502) as i64
            };
            return settle(
                record,
                plan,
                Outcome::failed(plan, code, error.message, None),
                &claim_id,
                &version,
            );
        }
    };
    // 等待验证时不持账号锁；发送前核对身份与租约，短暂持锁覆盖上游提交。
    let _lock = AccountLock::acquire()?;
    let current = account::record()?;
    if current["secret"] != record["secret"] {
        return Err(Error::http(409, "ZCode 账号已变化，未发送领取请求"));
    }
    let mut ledger = Ledger::load(&current)?;
    if ledger
        .plans
        .get(&plan.plan_id)
        .and_then(|saved| saved.in_flight_id.as_deref())
        != Some(claim_id.as_str())
    {
        return Err(Error::http(409, "ZCode 领取任务已变化，未发送领取请求"));
    }
    if let Some(saved) = ledger.plans.get_mut(&plan.plan_id) {
        saved.submitted_at = Some(sdk::now());
        saved.submitted_plan = Some(plan.clone());
    }
    ledger.save()?;
    let outcome = match upstream(record, plan, proof.as_ref(), &version) {
        Ok(outcome) => outcome,
        Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
        Err(error) => Outcome::failed(
            plan,
            error.status.unwrap_or(502) as i64,
            "领取请求未取得确定结果，已暂停重发；请稍后查询套餐状态",
            None,
        ),
    };
    drop(_lock);
    settle(record, plan, outcome, &claim_id, &version)
}

fn upstream(
    record: &Value,
    plan: &Plan,
    proof: Option<&(String, String)>,
    version: &str,
) -> Result<Outcome> {
    let secret = &record["secret"];
    let jwt = account::jwt(secret);
    if jwt.is_empty() {
        return Ok(Outcome::failed(plan, 401, "ZCode 账号缺少套餐 JWT", None));
    }
    let mut headers = config::billing_headers(&jwt, &account::mid(secret));
    headers["Content-Type"] = json!("application/json");
    headers["X-ZCode-App-Version"] = json!(version);
    headers["X-Platform"] = json!(config::PLATFORM);
    headers["X-Client-Language"] = json!("zh-CN");
    if let Some((param, region)) = proof {
        if !param.trim().is_empty() {
            headers["X-Aliyun-Captcha-Verify-Param"] = json!(param);
        }
        if !region.trim().is_empty() {
            headers["X-Aliyun-Captcha-Verify-Region"] = json!(region);
        }
    }
    let reply = network::request(
        "POST",
        &format!("{ZCODE}/api/v1/zcode-plan/billing/claim"),
        headers,
        Some(&json!({ "plan_id": plan.plan_id })),
    )?;
    let code = integer(&reply.value["code"]);
    let result = &reply.value["data"]["plan"];
    if (200..300).contains(&reply.status) && code == Some(0) && reply.value["success"] != false {
        return Ok(Outcome {
            claimed: true,
            plan_id: plan.plan_id.clone(),
            failure: String::new(),
            code: 0,
            message: "套餐领取成功".into(),
            starts_at: integer(&result["starts_at"]),
            ends_at: integer(&result["ends_at"]),
            retry_at: None,
        });
    }
    let code = code.unwrap_or(reply.status as i64);
    let message = match code {
        1001 => "套餐不存在",
        1002 => "活动已结束或尚未开始",
        1003 => "该账号已领取过",
        1004 => "账号或客户端版本不符合活动资格",
        1005 => "当日领取额度已用尽",
        3001 => "上游参数校验失败，请核对设备标识",
        3007 => "官方验证码未通过，请重新完成验证",
        3012 => "上游风控拒绝，已停止本轮后续领取",
        401 => "登录态已失效，请重新登录",
        _ => "上游未确认领取成功，请稍后查询套餐状态",
    };
    Ok(Outcome::failed(
        plan,
        code,
        message,
        integer(&result["ends_at"]),
    ))
}

fn settle(
    record: &Value,
    plan: &Plan,
    mut outcome: Outcome,
    claim_id: &str,
    version: &str,
) -> Result<Attempt> {
    let _lock = AccountLock::acquire()?;
    let current = account::record()?;
    if current["secret"] != record["secret"] {
        return Err(Error::http(409, "账号已变化，领取结果未写入另一份账号凭据"));
    }
    let mut ledger = Ledger::load(record)?;
    let saved = ledger
        .plans
        .get_mut(&plan.plan_id)
        .ok_or_else(|| Error::new("领取台账目标丢失"))?;
    if saved.in_flight_id.as_deref() != Some(claim_id) {
        return Err(Error::http(409, "领取台账任务已变化，请查询结果"));
    }
    saved.in_flight_at = None;
    saved.in_flight_id = None;
    if outcome.failure != "unknown" {
        saved.submitted_at = None;
        saved.submitted_plan = None;
    }
    if outcome.settled() {
        saved.claimed_at = Some(sdk::now());
        saved.claim_starts_at = outcome.starts_at;
        saved.claim_ends_at = outcome.ends_at;
        saved.retry = None;
        saved.rejection = None;
        ledger.checkin_verification = None;
    } else {
        let delay = if outcome.failure == "ineligible" {
            900_000
        } else {
            600_000
        };
        outcome.retry_at = Some(
            outcome
                .ends_at
                .filter(|_| matches!(outcome.failure.as_str(), "quota_exhausted" | "unavailable"))
                .map(|at| at.saturating_mul(1000))
                .unwrap_or(0)
                .max(sdk::now() + delay),
        );
        saved.retry = Some(outcome.clone());
        if outcome.failure == "ineligible" {
            saved.rejection = Some(Rejection {
                version: version.into(),
                at: sdk::now(),
                message: outcome.message.clone(),
            });
        }
        if outcome.failure == "captcha" {
            ledger.checkin_verification = Some(VerificationRequired {
                at: sdk::now(),
                plan_id: plan.plan_id.clone(),
                message: outcome.message.clone(),
            });
        }
    }
    ledger.add_recent_targets();
    let saved = ledger.save();
    let mut warning = saved.err().map(|error| error.message);
    if outcome.settled() && warning.is_none() {
        warning = match confirm(record, plan, &mut ledger, version) {
            Ok(warning) => warning,
            Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
            Err(error) => Some(error.message),
        };
    }
    Ok(projected(outcome, &ledger, warning))
}

fn finish_existing(
    record: &Value,
    plan: &Plan,
    outcome: Outcome,
    ledger: &mut Ledger,
    version: &str,
) -> Result<Attempt> {
    let warning = if outcome.settled() {
        if let Some(saved) = ledger.plans.get_mut(&plan.plan_id) {
            saved.plan.merge(plan.clone());
        }
        ledger.add_recent_targets();
        if let Err(error) = ledger.save() {
            return Ok(projected(outcome, ledger, Some(error.message)));
        }
        match confirm(record, plan, ledger, version) {
            Ok(warning) => warning,
            Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
            Err(error) => Some(error.message),
        }
    } else {
        None
    };
    Ok(projected(outcome, ledger, warning))
}

fn confirm(
    record: &Value,
    plan: &Plan,
    ledger: &mut Ledger,
    version: &str,
) -> Result<Option<String>> {
    let campaigns = ledger
        .plans
        .get(&plan.plan_id)
        .map(|saved| {
            saved
                .plan
                .marketing_campaign_ids
                .iter()
                .filter(|id| !saved.confirmed_campaigns.contains(id))
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut warning = None;
    for campaign in campaigns {
        if !plan.marketing_campaign_ids.contains(&campaign) {
            continue;
        }
        if ledger
            .plans
            .get(&plan.plan_id)
            .and_then(|saved| saved.campaign_attempts.get(&campaign))
            .is_some()
        {
            warning = Some("套餐已领取，营销回执仍在等待核对，暂不重复提交".into());
            continue;
        }
        if let Some(saved) = ledger.plans.get_mut(&plan.plan_id) {
            saved.campaign_attempts.insert(campaign.clone(), sdk::now());
        }
        ledger.save()?;
        match claim_sources::confirm(record, version, &campaign) {
            Ok(()) => {
                if let Some(saved) = ledger.plans.get_mut(&plan.plan_id) {
                    saved.campaign_attempts.remove(&campaign);
                    saved.confirmed_campaigns.push(campaign);
                }
                if let Err(error) = ledger.save() {
                    warning = Some(error.message);
                    break;
                }
            }
            Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
            Err(_) => {
                warning = Some("套餐已领取，但营销回执未完成；本轮不重复提交".into());
                break;
            }
        }
    }
    Ok(warning)
}
