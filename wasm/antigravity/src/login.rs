//! OAuth state 使用宿主 login_ + 32随机字节hex，新流程将 S256 verifier 仅保存在本次登录状态。
//! 唯一回调路径 /oauth-callback。预约换证后清理敏感材料，取消/并发 CAS 拒绝迟到落库。

use super::{account, callback, credentials, i18n, oauth, project, util};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{Scope, Snapshot},
    Error, Result, Value,
};

const KEY: &str = "oauth";
const TTL: i64 = 900_000;
fn flow(params: &Value) -> Result<&str> {
    params["flow"]
        .as_str()
        .filter(|f| {
            f.strip_prefix("login_")
                .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        })
        .ok_or_else(|| Error::http(503, "Antigravity 需要主程序分配受控登录流程"))
}
fn load() -> Result<Snapshot> {
    let state = sdk::state::get(Scope::Login, KEY, 1)?;
    if !state.compatible {
        return Err(Error::http(409, "Antigravity 登录状态已变化，请重新发起"));
    }
    Ok(state)
}
fn active(state: &Snapshot) -> Result<()> {
    if !matches!(state.value["phase"].as_str(), Some("pending" | "exchange"))
        || state.value["deadline"]
            .as_i64()
            .is_none_or(|at| util::now().map_or(true, |now| now >= at))
    {
        return Err(Error::http(409, "Antigravity 登录已取消、完成或过期"));
    }
    Ok(())
}
fn save(previous: &Snapshot, value: Value) -> Result<()> {
    if previous.value["deadline"]
        .as_i64()
        .is_some_and(|at| util::now().map_or(true, |now| now >= at))
    {
        return Err(Error::http(409, "Antigravity 登录已过期，未写入迟到结果"));
    }
    if !sdk::state::compare_exchange(Scope::Login, KEY, 1, previous.revision, value, TTL)? {
        return Err(Error::http(409, "Antigravity 登录步骤已变化，未重复换证"));
    }
    Ok(())
}
fn current(previous: &Snapshot) -> Result<()> {
    let state = load()?;
    active(&state)?;
    if state.revision != previous.revision {
        return Err(Error::http(409, "Antigravity 登录已取消或步骤变化"));
    }
    Ok(())
}

pub fn start(params: &Value) -> Result<Value> {
    let flow = flow(params)?;
    if params["payload"]["methodIndex"]
        .as_u64()
        .is_some_and(|index| index != 0)
        || params["method"].as_str().is_some_and(|m| m != "browser")
    {
        return Err(Error::http(
            400,
            "Antigravity 请使用网页登录；手工续期凭据请从添加账号提交",
        ));
    }
    let trusted = params["callback_url"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::http(503, "主程序没有提供可信的本机 OAuth 回调地址"))?;
    let redirect = callback::redirect(trusted)?;
    let previous = load()?;
    if !previous.value.is_null() {
        return Err(Error::http(409, "Antigravity 登录流程已存在"));
    }
    let deadline = util::now()?.saturating_add(TTL);
    let verifier = util::random_hex(32)?;
    let url = oauth::authorize(&redirect, flow, &verifier)?;
    let payload = params["payload"]
        .get("inputs")
        .filter(|value| value.is_object())
        .unwrap_or(&params["payload"]);
    let name = util::safe_text(&util::text(payload, &["name"]), 100);
    let project = util::header(&util::text(payload, &["projectId", "project_id"]), 1024)?;
    let locale = params["payload"]["locale"].as_str().unwrap_or("zh-CN");
    save(
        &previous,
        json!({ "phase": "pending", "deadline": deadline, "state": flow,
        "redirect": redirect, "verifier": verifier, "name": name, "projectId": project }),
    )?;
    Ok(json!({ "flow": flow, "url": url, "poll_interval_ms": 0,
        "instructions": i18n::instructions(locale),
        "external_browser": false, "allowed_hosts": [],
        "login_ui": { "browserMode": "choice", "manualCallback": true, "callbackInput": "url",
            "captureCallback": true, "cookieCapture": null, "loopbackTargets": [redirect] } }))
}

pub fn callback(params: &Value) -> Result<Value> {
    let flow = flow(params)?;
    let previous = load()?;
    active(&previous)?;
    if previous.value["phase"] != "pending" || previous.value["state"] != flow {
        return Err(Error::http(409, "Antigravity 授权已在处理，不能重复提交"));
    }
    let redirect = previous.value["redirect"]
        .as_str()
        .ok_or_else(|| Error::http(409, "登录回调地址已失效"))?;
    let received = match callback::parse(params["url"].as_str().unwrap_or(""), redirect, flow) {
        Ok(received) => received,
        Err(error) => {
            return Ok(json!({ "state": "pending", "retryable": true, "hint": error.message }))
        }
    };
    if received.denied {
        save(&previous, json!({ "phase": "done" }))?;
        return Ok(json!({ "state": "failed", "message": "Google 授权已取消或被拒绝" }));
    }
    let code = received
        .code
        .as_deref()
        .ok_or_else(|| Error::http(400, "Google 回调没有授权码"))?;
    let marker = util::random_hex(24)?;
    save(
        &previous,
        json!({ "phase": "exchange", "marker": marker, "deadline": previous.value["deadline"] }),
    )?;
    let reserved = load()?;
    if reserved.value["marker"] != marker {
        return Err(Error::http(409, "Antigravity 换证预约已失效"));
    }
    let result = (|| {
        current(&reserved)?;
        let mut secret = oauth::exchange(code, redirect, previous.value["verifier"].as_str())?;
        current(&reserved)?;
        let token = credentials::token(&secret)?;
        if let Some(email) = oauth::email(&token, || current(&reserved))? {
            secret["email"] = json!(email);
        }
        current(&reserved)?;
        let provided_project = util::text(&previous.value, &["projectId"]);
        if !provided_project.is_empty() {
            secret["projectId"] = json!(provided_project);
        } else {
            let found = project::discover(&token, || current(&reserved));
            current(&reserved)?;
            if let Ok(project) = found {
                secret["projectId"] = json!(project);
            }
        }
        account::normalized(secret, "web-login", previous.value["name"].as_str())
    })();
    current(&reserved)?;
    save(&reserved, json!({ "phase": "done" }))?;
    match result {
        Ok(account) => Ok(sdk::login::done(account)),
        Err(error) => {
            Ok(json!({ "state": "failed", "message": error.message, "code": error.code }))
        }
    }
}

pub fn poll(params: &Value) -> Result<Value> {
    flow(params)?;
    let state = load()?;
    Ok(if active(&state).is_ok() {
        json!({ "state": "pending", "hint": "等待 Google 授权回调或换证完成" })
    } else {
        json!({ "state": "failed", "message": "Antigravity 登录流程已结束或过期" })
    })
}
pub fn cancel(params: &Value) -> Result<Value> {
    flow(params)?;
    for _ in 0..3 {
        let state = load()?;
        if sdk::state::compare_exchange(
            Scope::Login,
            KEY,
            1,
            state.revision,
            json!({ "phase": "cancelled" }),
            TTL,
        )? {
            return Ok(Value::Null);
        }
    }
    Err(Error::http(409, "Antigravity 登录状态仍在变化，请稍后取消"))
}
