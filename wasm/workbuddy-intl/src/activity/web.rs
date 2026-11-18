//! 网页agent会话才是国际日活通道；证据aimod-cc/Agent2API#277及原生keepalive.rs。
//! 固定Hi与原conversationOrigin/plugins协议形状，不接用户任意工具/付款参数，不调用支付接口。
//! 无服务端幂等键证据：提交前记pending，网络结果不明绝不换模型再建收费会话。
use super::Context;
use crate::{catalog, errors, web_session};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

fn uncertain() -> Error {
    Error::http(
        502,
        "国际网页会话结果未确认，未换模型重复创建；请到官方页面核对",
    )
    .with_code("activity_web_outcome_uncertain")
}
pub(super) fn run(ctx: &mut Context, models: &[String]) -> Result<Value> {
    let before = ctx.get("web")?;
    if before["day"] == ctx.day && before["done"] == true {
        let mut result = before["result"].clone();
        result["reused"] = json!(true);
        return Ok(result);
    }
    if before["day"] == ctx.day && before["pending"] == true {
        return Err(uncertain());
    }
    ctx.check()?;
    let mut cookie = match web_session::ready(&ctx.current, &ctx.job) {
        Err(error) if error.status == Some(401) && !ctx.refreshed => {
            ctx.retry_auth()?;
            web_session::ready(&ctx.current, &ctx.job)?
        }
        result => result?,
    };
    let mut failures = Vec::new();
    for model in models {
        ctx.check()?;
        match catalog::check(&ctx.current, model) {
            Err(error) => {
                failures.push(json!({ "model": model, "msg": error.message }));
                continue;
            }
            Ok(Some(row)) if row["kind"] == "image" => {
                failures.push(json!({ "model": model, "msg": "图片模型不能用于网页日活" }));
                continue;
            }
            _ => {}
        }
        for attempt in 0..2 {
            ctx.check()?;
            // 原生明确带这个内置插件声明；仅注册，不包含订单、金额、支付或协议确认动作。
            let body = json!({ "prompt": "Hi", "model": model, "conversationOrigin": "workbuddy-app",
                "plugins": [{ "name": "weixinpay", "marketplace": "codebuddy-builtin" }] });
            ctx.put(
                "web",
                &json!({ "day": ctx.day, "pending": true, "model": model, "at": sdk::now() }),
            )?;
            let reply = web_session::request(&ctx.current, &ctx.job, &cookie, &body)
                .map_err(|_| uncertain())?;
            if reply.status == 401 || reply.status == 403 || reply.code() == Some(12153) {
                ctx.put(
                    "web",
                    &json!({ "day": ctx.day, "pending": false, "status": reply.status }),
                )?;
                if attempt == 0 && reply.status != 403 {
                    cookie = match web_session::renew(&ctx.current, &ctx.job, &cookie) {
                        Err(error) if error.status == Some(401) && !ctx.refreshed => {
                            ctx.retry_auth()?;
                            web_session::ready(&ctx.current, &ctx.job)?
                        }
                        result => result?,
                    };
                    continue;
                }
                return Err(Error::http(
                    reply.status.max(401),
                    "国际网页会话被拒绝，请核对官方登录或访问权限",
                ));
            }
            let code_ok = reply
                .value
                .get("code")
                .is_none_or(|v| v.is_null() || crate::util::integer(v) == Some(0));
            let id = reply.data()["id"].as_str().filter(|id| {
                !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
            });
            if reply.ok() && code_ok && id.is_some() {
                let mut result = json!({ "succeeded": true, "conversationId": id, "model": model, "at": sdk::now(),
                    "msg": "国际网页会话已创建；是否奖励到账需另行核对", "failures": failures });
                if ctx.put("web", &json!({ "day": ctx.day, "pending": false, "done": true, "result": result })).is_err() {
                    crate::util::warning(&mut result, "网页会话已创建，但完成台账未保存，不自动重复创建");
                }
                return Ok(result);
            }
            let rejected = (400..500).contains(&reply.status) && reply.status != 408
                || (200..300).contains(&reply.status) && reply.code().is_some_and(|code| code != 0);
            if !rejected {
                return Err(uncertain());
            }
            ctx.put("web", &json!({ "day": ctx.day, "pending": false, "model": model, "status": reply.status }))?;
            let message = errors::clean(reply.message(), &ctx.current.secrets());
            failures.push(json!({ "model": model, "status": reply.status, "msg": message }));
            if errors::policy(reply.status, &reply.value)["suspendAccount"] == true
                || errors::classify(reply.status, &reply.value)["kind"] == "content_blocked"
            {
                return Err(errors::upstream(
                    reply.status,
                    &reply.value,
                    &ctx.current.secrets(),
                ));
            }
            break;
        }
    }
    Ok(
        json!({ "succeeded": false, "conversationId": null, "model": null, "failures": failures,
        "msg": "授权模型链本轮未能创建国际网页会话" }),
    )
}
