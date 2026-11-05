//! ZCode 上游业务错误；风控拒绝必须先于限额、凭证和内容策略判定。

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::RetryDecision;
use serde_json::Value;

use super::{captcha, plan};

pub(super) fn classify(status: u16, body: &Value) -> proto::ErrorClass {
    let raw = message(body).unwrap_or(if status == 401 {
        "凭证被上游拒绝（套餐池通道认套餐 JWT、编码套餐认访问令牌）：\
         请在「账号」页重新登录该账号"
    } else {
        "上游错误"
    });
    let code = upstream_code(body);
    let risk_rejected = is_risk_rejection(status, body);
    if code == Some(3007) {
        captcha::note_challenge_and_require_token();
    }
    let hint = if risk_rejected {
        plan::code_hint(3012)
    } else {
        code.and_then(plan::code_hint).or_else(|| {
            (status == 429 || code == Some(1005)).then(|| plan::quota_hint(raw)).flatten()
        })
    };
    let message = match hint {
        Some(hint) => format!("上游返回 {status}: {raw}（{hint}）"),
        None => format!("上游返回 {status}: {raw}"),
    };
    let failure_status = if (200..300).contains(&status) { 502 } else { status };
    if risk_rejected {
        return proto::ErrorClass::Fatal {
            status: failure_status,
            message,
            upstream_code: code,
        };
    }
    if status == 401 {
        return proto::ErrorClass::TokenExpired { message };
    }
    if status == 429 || code == Some(1005) {
        return proto::ErrorClass::QuotaLimited {
            reset_at: None,
            message,
            upstream_code: code,
            status: 429,
        };
    }
    crate::content_block::classify_or_fatal(failure_status, body, message, code)
}

pub(super) fn retry_decision(status: u16, body: &Value) -> RetryDecision {
    if is_risk_rejection(status, body) {
        return RetryDecision::Stop {
            reason: "上游风控拒绝，停止本次请求，不再原地重发、刷新凭证或换账号".to_string(),
        };
    }
    RetryDecision::Default
}

fn upstream_code(body: &Value) -> Option<i64> {
    [body.get("code"), body.pointer("/error/code")]
        .into_iter()
        .flatten()
        .find_map(|value| {
            value.as_i64().or_else(|| value.as_str()?.trim().parse::<i64>().ok())
        })
}

fn message(body: &Value) -> Option<&str> {
    [body.get("message"), body.get("msg"), body.pointer("/error/message"), body.pointer("/error/msg")]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .find(|text| !text.is_empty())
        .or_else(|| body.as_str().map(str::trim).filter(|text| !text.is_empty()))
}

fn is_risk_rejection(status: u16, body: &Value) -> bool {
    match upstream_code(body) {
        Some(code) => code == 3012,
        None => {
            // 只有上游明确的风控原文才能补判；普通 405 不能等同于账号受限。
            matches!(status, 403 | 405)
                && message(body).is_some_and(|text| {
                    text.to_ascii_lowercase()
                        .contains("request has been blocked due to unusual activity")
                })
        }
    }
}
