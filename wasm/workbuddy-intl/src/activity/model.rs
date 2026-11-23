//! 国际桌面保活只用主程序授权的模型链，不设置默认免费链，不在领取模式调用模型。
//! 探测上限兼顾推理型模型（256），正文或思考内容完整收尾均视作最小对话已确认；
//! 明确拒绝或空收尾不清阻塞、记入失败明细后继续尝试下一个模型，仅网络/流中断暂缓 5 分钟防重。
use super::Context;
use crate::{catalog, errors, headers, network};
use agent2api_wasm_sdk::{
    self as sdk, json, protocol::Converter, sse::SseDecoder, Error, Result, Value,
};

const PROBE_MAX_TOKENS: u64 = 256;

fn uncertain(detail: Option<&str>) -> Error {
    let message = match detail.map(str::trim).filter(|text| !text.is_empty()) {
        Some(detail) => format!("国际保活模型调用结果未确认（{detail}），5 分钟内暂缓切换模型重复消费"),
        None => "国际保活模型调用结果未确认，5 分钟内暂缓切换模型重复消费".to_string(),
    };
    Error::http(502, message).with_code("activity_model_outcome_uncertain")
}
fn record_pending(ctx: &Context, model: &str, reason: &str) {
    let _ = ctx.put(
        "model",
        &json!({ "day": ctx.day, "pending": true, "model": model, "at": sdk::now(), "reason": reason }),
    );
}
pub(super) fn run(ctx: &mut Context, models: &[String]) -> Result<Value> {
    let before = ctx.get("model")?;
    if before["day"] == ctx.day && before["done"] == true {
        let mut result = before["result"].clone();
        result["reused"] = json!(true);
        return Ok(result);
    }
    if super::pending_active(&before, &ctx.day, Some(models)) {
        return Err(uncertain(super::pending_detail(&before).as_deref()));
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
            let body = json!({ "model": model, "stream": true, "max_tokens": PROBE_MAX_TOKENS, "messages": [
                { "role": "system", "content": "You are a helpful assistant." }, { "role": "user", "content": "hi" }
            ] });
            let h = headers::chat(&ctx.current.secret)?;
            ctx.put(
                "model",
                &json!({ "day": ctx.day, "pending": true, "model": model, "at": sdk::now() }),
            )?;
            let response = match network::open_timeout(
                "POST",
                &ctx.current.endpoint()?,
                "/v2/chat/completions",
                h,
                Some(&body),
                ctx.job.remaining_ms()?.min(20_000),
            ) {
                Ok(response) => response,
                Err(error) => {
                    let reason = errors::clean(&error.message, &ctx.current.secrets());
                    record_pending(ctx, model, &reason);
                    return Err(uncertain(Some(&format!("{model}：{reason}"))));
                }
            };
            if !(200..300).contains(&response.status)
                || response
                    .header_values("content-type")
                    .iter()
                    .any(|s| s.to_ascii_lowercase().contains("application/json"))
            {
                let reply = match network::read(response, 1024 * 1024) {
                    Ok(reply) => reply,
                    Err(error) => {
                        let reason = errors::clean(&error.message, &ctx.current.secrets());
                        record_pending(ctx, model, &reason);
                        return Err(uncertain(Some(&format!("{model}：{reason}"))));
                    }
                };
                let known_rejection = ((400..500).contains(&reply.status) && reply.status != 408)
                    || (200..300).contains(&reply.status)
                        && reply.code().is_some_and(|code| code != 0);
                if !known_rejection {
                    let reason = format!(
                        "HTTP {}：{}",
                        reply.status,
                        errors::clean(reply.message(), &ctx.current.secrets())
                    );
                    record_pending(ctx, model, &reason);
                    return Err(uncertain(Some(&format!("{model}：{reason}"))));
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
            match consume(ctx, response, model) {
                ConsumeOutcome::Succeeded { usage, content } => {
                    let msg = if content {
                        "授权模型已完成最小桌面对话"
                    } else {
                        "授权模型已完成最小桌面对话（思考内容有效，正文为空）"
                    };
                    let mut result = json!({ "succeeded": true, "model": model, "usage": usage, "at": sdk::now(),
                        "msg": msg, "failures": failures });
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
                ConsumeOutcome::EmptyDone => {
                    ctx.put("model", &json!({ "day": ctx.day, "pending": false, "model": model, "status": 200 }))?;
                    failures.push(
                        json!({ "model": model, "status": 200, "message": "模型返回空内容（无正文与思考内容）" }),
                    );
                    break;
                }
                ConsumeOutcome::UpstreamError(error) => {
                    ctx.put("model", &json!({ "day": ctx.day, "pending": false, "model": model, "status": error.status.unwrap_or(200) }))?;
                    let is_auth = error.status == Some(401) || error.upstream_code == Some(12153);
                    if is_auth && attempt == 0 && !ctx.refreshed {
                        ctx.retry_auth()?;
                        continue;
                    }
                    if is_auth || error.code.as_deref() == Some("workbuddy_content_blocked") {
                        return Err(error);
                    }
                    failures.push(
                        json!({ "model": model, "status": error.status.unwrap_or(200), "message": error.message }),
                    );
                    break;
                }
                ConsumeOutcome::Uncertain(reason) => {
                    record_pending(ctx, model, &reason);
                    return Err(uncertain(Some(&format!("{model}：{reason}"))));
                }
            }
        }
    }
    Ok(
        json!({ "succeeded": false, "model": null, "failures": failures, "msg": "授权模型链本轮未完成桌面保活" }),
    )
}
enum ConsumeOutcome {
    Succeeded { usage: Value, content: bool },
    EmptyDone,
    UpstreamError(Error),
    Uncertain(String),
}
enum ObserveFailure {
    Upstream(Error),
    Uncertain(String),
}
fn consume(
    ctx: &Context,
    mut response: agent2api_wasm_sdk::http::Response,
    model: &str,
) -> ConsumeOutcome {
    let mut converter = match Converter::new("chat", model) {
        Ok(converter) => converter,
        Err(error) => return ConsumeOutcome::Uncertain(error.message),
    };
    let mut decoder = SseDecoder::default();
    let mut answer = Answer::default();
    let mut bytes_total = 0usize;
    loop {
        if let Err(error) = ctx.check() {
            return ConsumeOutcome::Uncertain(error.message);
        }
        let bytes = match response.read() {
            Ok(Some(bytes)) => bytes,
            Ok(None) => break,
            Err(error) => return ConsumeOutcome::Uncertain(error.message),
        };
        bytes_total = bytes_total.saturating_add(bytes.len());
        if bytes_total > 1024 * 1024 {
            return ConsumeOutcome::Uncertain("流式响应超过 1 MiB".to_string());
        }
        let converted = match converter.push(&bytes) {
            Ok(bytes) => bytes,
            Err(error) => return ConsumeOutcome::Uncertain(error.message),
        };
        let events = match decoder.push(&converted) {
            Ok(events) => events,
            Err(error) => return ConsumeOutcome::Uncertain(error.message),
        };
        for event in events {
            match answer.observe(&event, ctx) {
                Ok(()) => {}
                Err(ObserveFailure::Upstream(error)) => return ConsumeOutcome::UpstreamError(error),
                Err(ObserveFailure::Uncertain(reason)) => return ConsumeOutcome::Uncertain(reason),
            }
        }
        if answer.done {
            break;
        }
    }
    let tail = match converter.finish() {
        Ok(bytes) => bytes,
        Err(error) => return ConsumeOutcome::Uncertain(error.message),
    };
    let mut events = match decoder.push(&tail) {
        Ok(events) => events,
        Err(error) => return ConsumeOutcome::Uncertain(error.message),
    };
    match decoder.push(b"\n\n") {
        Ok(rest) => events.extend(rest),
        Err(error) => return ConsumeOutcome::Uncertain(error.message),
    }
    for event in events {
        match answer.observe(&event, ctx) {
            Ok(()) => {}
            Err(ObserveFailure::Upstream(error)) => return ConsumeOutcome::UpstreamError(error),
            Err(ObserveFailure::Uncertain(reason)) => return ConsumeOutcome::Uncertain(reason),
        }
    }
    if !answer.done {
        return ConsumeOutcome::Uncertain(if answer.content || answer.reasoning {
            "流式响应缺少结束标记".to_string()
        } else {
            "未收到完整流式响应".to_string()
        });
    }
    if !answer.content && !answer.reasoning {
        return ConsumeOutcome::EmptyDone;
    }
    ConsumeOutcome::Succeeded {
        usage: answer.usage,
        content: answer.content,
    }
}
#[derive(Default)]
struct Answer {
    content: bool,
    reasoning: bool,
    done: bool,
    usage: Value,
}
impl Answer {
    fn observe(&mut self, event: &str, ctx: &Context) -> std::result::Result<(), ObserveFailure> {
        if self.done {
            return Err(ObserveFailure::Uncertain("结束后出现额外事件".to_string()));
        }
        if event.trim() == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let value: Value = serde_json::from_str(event)
            .map_err(|_| ObserveFailure::Uncertain("流式事件不是合法 JSON".to_string()))?;
        if errors::failed(200, &value) {
            return Err(ObserveFailure::Upstream(errors::upstream(
                200,
                &value,
                &ctx.current.secrets(),
            )));
        }
        for choice in value["choices"].as_array().into_iter().flatten() {
            self.content |= choice["delta"]["content"]
                .as_str()
                .is_some_and(|s| !s.trim().is_empty());
            self.reasoning |= ["reasoning_content", "reasoning"].iter().any(|key| {
                choice["delta"][*key]
                    .as_str()
                    .is_some_and(|s| !s.trim().is_empty())
            });
        }
        if value["usage"].is_object() {
            self.usage = value["usage"].clone();
        }
        Ok(())
    }
}
