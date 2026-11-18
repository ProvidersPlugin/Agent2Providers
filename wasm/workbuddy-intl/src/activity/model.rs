//! 国际桌面保活只用主程序授权的模型链，不设置默认免费链，不在领取模式调用模型。
//! 原生最小对话形状保留；更严格要求可读正文及真实DONE，HTTP200错误不能算保活成功。
use super::Context;
use crate::{catalog, errors, headers, network};
use agent2api_wasm_sdk::{
    self as sdk, json, protocol::Converter, sse::SseDecoder, Error, Result, Value,
};

fn uncertain() -> Error {
    Error::http(502, "国际保活模型调用结果未确认，未切换模型重复消费")
        .with_code("activity_model_outcome_uncertain")
}
pub(super) fn run(ctx: &mut Context, models: &[String]) -> Result<Value> {
    let before = ctx.get("model")?;
    if before["day"] == ctx.day && before["done"] == true {
        let mut result = before["result"].clone();
        result["reused"] = json!(true);
        return Ok(result);
    }
    if before["day"] == ctx.day && before["pending"] == true {
        return Err(uncertain());
    }
    let mut failures = Vec::new();
    for model in models {
        ctx.check()?;
        match catalog::check(&ctx.current, model) {
            Err(error) => {
                failures.push(json!({ "model": model, "message": error.message }));
                continue;
            }
            Ok(Some(row)) if row["kind"] == "image" => {
                failures.push(json!({ "model": model, "message": "该模型是图片模型，未用于保活" }));
                continue;
            }
            _ => {}
        }
        for attempt in 0..2 {
            ctx.check()?;
            let body = json!({ "model": model, "stream": true, "max_tokens": 16, "messages": [
                { "role": "system", "content": "You are a helpful assistant." }, { "role": "user", "content": "hi" }
            ] });
            let h = headers::chat(&ctx.current.secret)?;
            ctx.put(
                "model",
                &json!({ "day": ctx.day, "pending": true, "model": model, "at": sdk::now() }),
            )?;
            let response = network::open_timeout(
                "POST",
                &ctx.current.endpoint()?,
                "/v2/chat/completions",
                h,
                Some(&body),
                ctx.job.remaining_ms()?.min(20_000),
            )
            .map_err(|_| uncertain())?;
            if !(200..300).contains(&response.status)
                || response
                    .header_values("content-type")
                    .iter()
                    .any(|s| s.to_ascii_lowercase().contains("application/json"))
            {
                let reply = network::read(response, 1024 * 1024).map_err(|_| uncertain())?;
                let known_rejection = ((400..500).contains(&reply.status) && reply.status != 408)
                    || (200..300).contains(&reply.status)
                        && reply.code().is_some_and(|code| code != 0);
                if !known_rejection {
                    return Err(uncertain());
                }
                ctx.put("model", &json!({ "day": ctx.day, "pending": false, "model": model, "status": reply.status }))?;
                if super::authentication(&reply) && attempt == 0 && !ctx.refreshed {
                    ctx.retry_auth()?;
                    continue;
                }
                let error = errors::upstream(reply.status, &reply.value, &ctx.current.secrets());
                if errors::policy(reply.status, &reply.value)["suspendAccount"] == true
                    || errors::classify(reply.status, &reply.value)["kind"] == "content_blocked"
                    || super::authentication(&reply)
                {
                    return Err(error);
                }
                failures.push(
                    json!({ "model": model, "status": reply.status, "message": error.message }),
                );
                break;
            }
            let usage = consume(ctx, response, model).map_err(|_| uncertain())?;
            let mut result = json!({ "succeeded": true, "model": model, "usage": usage, "at": sdk::now(),
                "msg": "授权模型已完成最小桌面对话", "failures": failures });
            if ctx
                .put(
                    "model",
                    &json!({ "day": ctx.day, "done": true, "pending": false, "result": result }),
                )
                .is_err()
            {
                crate::util::warning(
                    &mut result,
                    "模型保活已返回完整结果，但台账未保存；未重复请求",
                );
            }
            return Ok(result);
        }
    }
    Ok(
        json!({ "succeeded": false, "model": null, "failures": failures, "msg": "授权模型链本轮未完成桌面保活" }),
    )
}
fn consume(
    ctx: &Context,
    mut response: agent2api_wasm_sdk::http::Response,
    model: &str,
) -> Result<Value> {
    let mut converter = Converter::new("chat", model)?;
    let mut decoder = SseDecoder::default();
    let mut answer = Answer::default();
    let mut bytes_total = 0usize;
    loop {
        ctx.check()?;
        let Some(bytes) = response.read()? else { break };
        bytes_total = bytes_total.saturating_add(bytes.len());
        if bytes_total > 1024 * 1024 {
            return Err(uncertain());
        }
        for event in decoder.push(&converter.push(&bytes)?)? {
            answer.observe(&event, ctx)?;
        }
        if answer.done {
            break;
        }
    }
    let mut events = decoder.push(&converter.finish()?)?;
    events.extend(decoder.push(b"\n\n")?);
    for event in events {
        answer.observe(&event, ctx)?;
    }
    if !answer.done || !answer.content {
        return Err(uncertain());
    }
    Ok(answer.usage)
}
#[derive(Default)]
struct Answer {
    content: bool,
    done: bool,
    usage: Value,
}
impl Answer {
    fn observe(&mut self, event: &str, ctx: &Context) -> Result<()> {
        if self.done {
            return Err(uncertain());
        }
        if event.trim() == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let value: Value = serde_json::from_str(event).map_err(|_| uncertain())?;
        if errors::failed(200, &value) {
            return Err(errors::upstream(200, &value, &ctx.current.secrets()));
        }
        for choice in value["choices"].as_array().into_iter().flatten() {
            self.content |= choice["delta"]["content"]
                .as_str()
                .is_some_and(|s| !s.trim().is_empty());
        }
        if value["usage"].is_object() {
            self.usage = value["usage"].clone();
        }
        Ok(())
    }
}
