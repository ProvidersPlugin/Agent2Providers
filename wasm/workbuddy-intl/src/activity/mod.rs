//! 国际日活与国内成长任务中心无关；保留原生探测/上报/领取/模型/网页会话顺序。
//! 网页日活证据为aimod-cc/Agent2API#277；仅创建会话成功不承诺次日积分到账。
//! 主程序rpc.rs在activity.run(非claim)先调用authorized_models并覆盖payload.models。
//! 本包不从目录或DEFAULT_FREE_MODELS挑备用模型，空授权链明确跳过；不新增默认排程。
mod billing;
mod model;
mod report;
mod web;

use crate::{
    headers,
    journal::{self, Job},
    network,
    session::{self, Session},
    util,
};
use agent2api_wasm_sdk::{self as sdk, json, AccountLock, Error, Result, Value};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Claim,
    Keepalive,
    Full,
}
impl Mode {
    fn parse(value: &Value) -> Result<Self> {
        match value.as_str().or_else(|| value.is_null().then_some("full")) {
            Some("claim") => Ok(Self::Claim),
            Some("keepalive") => Ok(Self::Keepalive),
            Some("full" | "") => Ok(Self::Full),
            _ => Err(Error::http(400, "国际日活模式只能为claim、keepalive或full")),
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Claim => "claim",
            Self::Keepalive => "keepalive",
            Self::Full => "full",
        }
    }
}
struct Context {
    current: Session,
    lock: AccountLock,
    job: Job,
    day: String,
    refreshed: bool,
}
impl Context {
    fn check(&self) -> Result<()> {
        self.job.check(&self.current)?;
        if self.day != util::day(sdk::now()) {
            return Err(
                Error::http(409, "国际日活已跨越账期日界，未继续以旧日状态提交")
                    .with_code("activity_day_changed"),
            );
        }
        Ok(())
    }
    fn get(&self, key: &str) -> Result<Value> {
        journal::get(&self.current, key)
    }
    fn put(&self, key: &str, value: &Value) -> Result<()> {
        self.job.checkpoint(&self.current)?;
        journal::put(&self.current, key, value)
    }
    fn retry_auth(&mut self) -> Result<()> {
        self.check()?;
        if self.refreshed {
            return Err(Error::http(401, "国际日活登录态仍被拒绝，请重新登录"));
        }
        self.refreshed = true;
        self.current = session::refresh_locked_after_rejection(&self.current, &self.lock)?;
        self.check()
    }
    fn call(&mut self, method: &str, path: &str, body: Option<&Value>) -> Result<network::Reply> {
        for attempt in 0..2 {
            self.check()?;
            let reply = network::raw_timeout(
                method,
                &self.current.endpoint()?,
                path,
                headers::framework(&self.current.secret)?,
                body,
                self.job.remaining_ms()?.min(20_000),
            )?;
            if authentication(&reply) && attempt == 0 && !self.refreshed {
                self.retry_auth()?;
                continue;
            }
            return Ok(reply);
        }
        Err(Error::http(401, "国际日活认证失败"))
    }
}
pub(super) const PENDING_TTL_MS: i64 = 5 * 60_000;

pub(super) fn pending_active(saved: &Value, day: &str, models: Option<&[String]>) -> bool {
    if saved["day"] != day || saved["pending"] != true {
        return false;
    }
    if !saved["at"]
        .as_i64()
        .is_some_and(|at| sdk::now().saturating_sub(at) < PENDING_TTL_MS)
    {
        return false;
    }
    match models {
        Some(list) => saved["model"]
            .as_str()
            .is_some_and(|model| list.iter().any(|item| item == model)),
        None => true,
    }
}
pub(super) fn pending_detail(saved: &Value) -> Option<String> {
    let model = saved["model"].as_str().map(str::trim).unwrap_or("");
    let reason = saved["reason"].as_str().map(str::trim).unwrap_or("");
    match (model.is_empty(), reason.is_empty()) {
        (false, false) => Some(format!("{model}：{reason}")),
        (false, true) => Some(model.to_string()),
        (true, false) => Some(reason.to_string()),
        (true, true) => None,
    }
}
fn step_failure_summary(step: &Value) -> Option<String> {
    let failures = step["failures"].as_array().filter(|rows| !rows.is_empty());
    if let Some(rows) = failures {
        let mut parts = Vec::new();
        for row in rows.iter().take(3) {
            let model = row["model"].as_str().map(str::trim).unwrap_or("");
            let msg = row["message"]
                .as_str()
                .or_else(|| row["msg"].as_str())
                .map(str::trim)
                .unwrap_or("");
            match (model.is_empty(), msg.is_empty()) {
                (false, false) => parts.push(format!("{model}: {msg}")),
                (false, true) => parts.push(model.to_string()),
                (true, false) => parts.push(msg.to_string()),
                (true, true) => {}
            }
        }
        if !parts.is_empty() {
            let suffix = if rows.len() > 3 {
                format!(" 等 {} 个模型失败", rows.len())
            } else {
                String::new()
            };
            return Some(format!("{}{suffix}", parts.join("；")));
        }
    }
    step["msg"]
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}
fn activity_summary(skipped: &Value, poke: &Value, web: &Value) -> Option<String> {
    if let Some(reason) = skipped.as_str().map(str::trim).filter(|s| !s.is_empty()) {
        return Some(format!("未调用保活模型：{reason}"));
    }
    let poke_ok = poke["succeeded"] == true;
    let web_ok = web["succeeded"] == true;
    if poke_ok && web_ok {
        return None;
    }
    let mut parts = Vec::new();
    if !poke_ok {
        if let Some(detail) = step_failure_summary(poke) {
            parts.push(format!("模型保活：{detail}"));
        }
    }
    if !web_ok && !(poke["uncertain"] == true && web["uncertain"] == true) {
        if let Some(detail) = step_failure_summary(web) {
            parts.push(format!("网页会话：{detail}"));
        }
    }
    (!parts.is_empty()).then(|| parts.join("；"))
}
fn authentication(reply: &network::Reply) -> bool {
    reply.status == 401 || reply.code() == Some(12153)
}
fn failure(error: Error) -> Value {
    json!({ "succeeded": false, "uncertain": error.code.as_deref().is_some_and(|code| code.contains("uncertain")),
        "status": error.status, "providerCode": error.code, "msg": error.message })
}
fn models(payload: &Value) -> Result<Vec<String>> {
    // 只可从主程序可信管理调用的models消费；tasks/checkin不带该授权链，不能旁路执行。
    let rows = payload["models"]
        .as_array()
        .ok_or_else(|| Error::http(403, "主程序未下发授权保活模型清单，未调用模型"))?;
    if rows.len() > 16 {
        return Err(Error::http(400, "授权保活链超过本轮16个模型护栏"));
    }
    let mut models = Vec::<String>::new();
    for value in rows {
        let model = value
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
            .ok_or_else(|| Error::http(400, "授权保活模型名称无效"))?;
        if !models.iter().any(|known| known == model) {
            models.push(model.to_string());
        }
    }
    if models.is_empty() {
        return Err(Error::http(
            403,
            "主程序授权模型链为空，未自行选择免费或付费模型；请在签到中心明确设置获准模型",
        ));
    }
    Ok(models)
}
pub fn status() -> Result<Value> {
    let current = session::ensure(false)?;
    billing::status_for(&current)
}
pub fn run(payload: &Value) -> Result<Value> {
    let started = sdk::now();
    let mode = Mode::parse(&payload["mode"])?;
    let authorized = if mode != Mode::Claim {
        Some(models(payload))
    } else {
        None
    };
    let lock = AccountLock::acquire()?;
    let current = session::ensure_locked(&lock)?;
    let job = Job::begin(&current, started.saturating_add(45_000))?;
    let mut ctx = Context {
        current,
        lock,
        job,
        day: util::day(sdk::now()),
        refreshed: false,
    };
    let status = if mode == Mode::Keepalive {
        Value::Null
    } else {
        billing::status(&mut ctx).unwrap_or_else(billing::status_error)
    };
    // 领取专用入口只探测与领取，不能冒充聊天事件上报来改变活跃状态。
    let report = if mode == Mode::Claim {
        Value::Null
    } else {
        report::run(&mut ctx).unwrap_or_else(failure)
    };
    let claim = if mode == Mode::Keepalive {
        json!({ "success": false, "outcome": "not_requested", "credit": null, "code": null, "msg": "本轮只保活，未探测或领取奖励" })
    } else {
        billing::claim(&mut ctx, &status).unwrap_or_else(|error| json!({ "success": false, "outcome": "failed",
            "credit": null, "code": error.upstream_code, "providerCode": error.code, "msg": error.message }))
    };
    let (poke, web, skipped) = match authorized {
        Some(Ok(models)) => {
            let poke = model::run(&mut ctx, &models).unwrap_or_else(failure);
            let web = if poke["uncertain"] == true {
                json!({ "succeeded": false, "uncertain": true, "msg": "因桌面模型结果未确认，本轮暂未追加网页模型消费" })
            } else {
                web::run(&mut ctx, &models).unwrap_or_else(failure)
            };
            (poke, web, Value::Null)
        }
        Some(Err(error)) => (Value::Null, Value::Null, json!(error.message)),
        None => (Value::Null, Value::Null, Value::Null),
    };
    let summary = if mode != Mode::Claim {
        activity_summary(&skipped, &poke, &web)
    } else {
        None
    };
    let mut value = json!({ "status": status, "claim": claim, "activity": {
        "mode": mode.name(), "active": status["active"], "todayCheckedIn": status["todayCheckedIn"], "statusAvailable": status["statusAvailable"],
        "report": report, "pokeSucceeded": poke["succeeded"], "pokeModel": poke["model"], "poke": poke, "webConversation": web,
        "modelCallsSkipped": !skipped.is_null(), "error": skipped, "summary": summary,
        "rewardConfirmed": claim["success"] == true || claim["alreadyCompleted"] == true,
        "webActivityConfirmed": web["succeeded"] == true,
        "message": "上报、桌面对话、网页会话与奖励到账为不同结果，请分别查看" }, "at": sdk::now() });
    if mode == Mode::Keepalive {
        value["claim"]["msg"] = json!(if web["succeeded"] == true {
            "保活完成，网页会话已创建；本轮未领取奖励".to_string()
        } else if poke["succeeded"] == true {
            match step_failure_summary(&web) {
                Some(reason) => format!("桌面保活已完成，但网页会话失败（{reason}）；本轮未领取奖励"),
                None => "桌面保活已完成，但网页日活未确认；本轮未领取奖励".to_string(),
            }
        } else {
            summary.unwrap_or_else(|| "网页日活与模型保活未确认，请查看具体原因".to_string())
        });
    }
    session::check(&ctx.current)?;
    journal::last(&ctx.current, &mut value);
    Ok(value)
}

// 安全的跨家领取RPC只走领取模式，不能跳过主程序模型授权；普通checkin能力保持false。
pub fn claim_only() -> Result<Value> {
    run(&json!({ "mode": "claim" }))
}
