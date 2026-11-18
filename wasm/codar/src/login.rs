//! 扫码、手机绑定和短信验证只使用当前宿主 flow；取消后的迟到凭据不落库。
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

use crate::{
    account, desktop, endpoints, login_phone as phone, login_state as state, network, util,
};

pub fn start(params: &Value) -> Result<Value> {
    let flow = state::flow(params)?;
    if params["method"]
        .as_str()
        .is_some_and(|method| !matches!(method, "browser" | "desktop"))
    {
        return Err(Error::http(400, "科搭登录方式无效"));
    }
    let index = if params["method"] == "desktop" {
        4
    } else {
        params["payload"]["methodIndex"].as_u64().unwrap_or(0)
    };
    let old = state::load()?;
    if !old.value.is_null() {
        return Err(Error::http(409, "科搭登录流程已经存在，请先完成或取消"));
    }
    let name = util::text(&params["payload"], &["name"]);
    let locale = params["payload"]["locale"].as_str().unwrap_or("zh-CN");
    let mut value = json!({ "deadline": util::now()?.saturating_add(state::TTL), "name": name, "locale": locale,
        "step": util::nonce()?, "methodIndex": index, "phase": "starting" });
    match index {
        0 => {
            let reply = network::request(
                "POST",
                endpoints::ACCOUNT,
                endpoints::QR,
                None,
                Some(&json!({})),
                12_000,
            )?
            .checked(&[])?;
            let data = network::data(&reply);
            let scene = data["scene"]
                .as_str()
                .filter(|scene| {
                    !scene.is_empty() && scene.len() <= 1024 && !scene.chars().any(char::is_control)
                })
                .ok_or_else(|| Error::http(502, "科搭没有返回有效扫码标识"))?;
            let raw = data["qr_url"]
                .as_str()
                .ok_or_else(|| Error::http(502, "科搭没有返回二维码地址"))?;
            let url = util::checked_url(raw)?;
            if !matches!(
                url.host_str(),
                Some("mp.weixin.qq.com" | "api.kuaipaper.com" | "api.claw.speedai.com")
            ) {
                return Err(Error::http(502, "科搭二维码地址不属于已确认的官方来源"));
            }
            value["phase"] = json!("qr");
            value["scene"] = json!(scene);
            value["qrUrl"] = json!(url.as_str());
            value["nextPollAt"] = json!(0);
        }
        1 => {
            value["phase"] = json!("phone");
            value["purpose"] = json!("login");
        }
        4 => {
            let normalized = desktop::normalized((!name.is_empty()).then_some(name.as_str()))?;
            value["phase"] = json!("done");
            value["account"] = normalized;
        }
        _ => return Err(Error::http(400, "请从科搭凭据导入入口提交令牌或 JSON")),
    }
    state::save(&old, value.clone())?;
    let mut result = json!({ "flow": flow, "url": "", "poll_interval_ms": 1500,
        "instructions": "请在当前登录步骤完成微信扫码或手机验证；登录完成后会在线核对账号身份。",
        "external_browser": false, "allowed_hosts": [], "login_ui": { "browserMode": "none", "manualCallback": false,
            "callbackInput": "url", "cookieCapture": null, "captureCallback": false } });
    if index != 4 {
        let interaction = phone::form(&value)?;
        result["instructions"] = interaction["message"].clone();
        result["interaction"] = interaction;
    }
    Ok(result)
}

pub fn submit(params: &Value) -> Result<Value> {
    state::flow(params)?;
    let old = state::load()?;
    state::live(&old)?;
    if old.value["step"] != params["interaction"] {
        return Err(Error::http(409, "科搭登录步骤已变化，请重新读取"));
    }
    match old.value["phase"].as_str() {
        Some("phone") => phone::send(&old, params),
        Some("code") => phone::verify(&old, params),
        _ => Err(Error::http(409, "当前科搭登录步骤不接受手工提交")),
    }
}

pub fn poll(params: &Value) -> Result<Value> {
    state::flow(params)?;
    let old = state::load()?;
    if old.value["phase"] == "done" {
        if old.value["deadline"].as_i64().unwrap_or(0) <= util::now()? {
            return Ok(json!({ "state": "failed", "message": "科搭登录已过期，请重新发起" }));
        }
        return Ok(sdk::login::done(old.value["account"].clone()));
    }
    if old.value["phase"] == "failed" {
        return Ok(json!({ "state": "failed", "message": old.value["message"] }));
    }
    if state::live(&old).is_err() {
        return Ok(json!({ "state": "failed", "message": "科搭登录已取消或过期，请重新发起" }));
    }
    match old.value["phase"].as_str() {
        Some("phone" | "code") => {
            Ok(json!({ "state": "pending", "interaction": phone::form(&old.value)? }))
        }
        Some("qr") => qr_poll(&old),
        Some("sending" | "verifying" | "polling")
            if old.value["inFlightUntil"].as_i64().unwrap_or(0) > util::now()? =>
        {
            // 后台轮询会与真实短信提交并行，不能把仍在等待上游的步骤误判成中断。
            Ok(sdk::login::pending(None))
        }
        Some("sending" | "verifying" | "polling") => phone::failure(
            &old,
            "上次登录步骤处理已中断，未重新提交验证码或读取一次性授权，请重新发起",
        ),
        _ => Err(Error::http(409, "科搭登录阶段无效，请重新发起")),
    }
}

fn qr_poll(old: &sdk::state::Snapshot) -> Result<Value> {
    if old.value["nextPollAt"].as_i64().unwrap_or(0) > util::now()? {
        return Ok(json!({ "state": "pending", "interaction": phone::form(&old.value)? }));
    }
    let reserved = state::reserve(old, "polling")?;
    let reply = network::request(
        "POST",
        endpoints::ACCOUNT,
        endpoints::QR_STATUS,
        None,
        Some(&json!({ "scene": old.value["scene"] })),
        8000,
    );
    state::unchanged(&reserved)?;
    let reply = match reply {
        Ok(reply) => reply,
        Err(_) => {
            let mut value = old.value.clone();
            value["nextPollAt"] = json!(util::now()?.saturating_add(2000));
            state::save(&reserved, value.clone())?;
            return Ok(json!({ "state": "pending", "interaction": phone::form(&value)? }));
        }
    };
    if let Some(next) = phone::require_phone(&reserved, &reply.body)? {
        return Ok(next);
    }
    let reply = match reply.checked(&[]) {
        Ok(reply) => reply,
        Err(error) => return phone::failure(&reserved, &error.message),
    };
    let data = network::data(&reply);
    let status = util::text(data, &["status"]).to_ascii_lowercase();
    let token = util::text(data, &["token"]);
    if matches!(status.as_str(), "" | "pending" | "scanned") && token.is_empty() {
        let mut value = old.value.clone();
        value["nextPollAt"] = json!(util::now()?.saturating_add(2000));
        state::save(&reserved, value.clone())?;
        return Ok(json!({ "state": "pending", "interaction": phone::form(&value)? }));
    }
    if status != "success" {
        return phone::failure(&reserved, "科搭扫码未完成，请重新发起登录");
    }
    let account = match account::from_token(&token, old.value["name"].as_str(), "wechat") {
        Ok(account) => account,
        Err(error) => return phone::failure(&reserved, &error.message),
    };
    state::unchanged(&reserved)?;
    state::save(
        &reserved,
        json!({ "phase": "done", "deadline": old.value["deadline"], "account": account }),
    )?;
    Ok(sdk::login::done(account))
}
