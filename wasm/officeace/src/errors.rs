//! 保留 81113→413 优先于429、模型 Basic 认证错误不误续 STS，以及控制面精准过期判据。

use super::credentials::Credentials;
use agent2api_wasm_sdk::{json, Error, Value};

pub const CONTROL_EXPIRED: &str = "officeace_control_token_expired";
pub const UNCERTAIN: &str = "officeace_subscription_uncertain";
pub const REFRESH_UNCERTAIN: &str = "officeace_refresh_uncertain";

pub fn account_changed() -> Error {
    Error::http(409, "OfficeAce 账号或模型凭据已变化，请重新发起操作")
        .with_code("officeace_account_changed")
}

pub fn clean(text: &str, secrets: &[&str]) -> String {
    let mut text = text.to_string();
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        text = text.replace(secret, "[已隐藏]");
    }
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(1200)
        .collect()
}

pub fn redact(error: &mut Error, secret: &Value) {
    fn visit(value: &Value, message: &mut String) {
        match value {
            Value::Object(values) => {
                for value in values.values() {
                    visit(value, message);
                }
            }
            Value::Array(values) => {
                for value in values {
                    visit(value, message);
                }
            }
            Value::String(value) if value.len() >= 8 => {
                *message = message.replace(value, "[已隐藏]")
            }
            _ => {}
        }
    }
    visit(secret, &mut error.message);
    error.message = error.message.chars().take(1200).collect();
}

pub fn public_payload(payload: &Value, secret: &Value) -> Value {
    fn scrub(value: &Value, depth: usize) -> Value {
        if depth > 24 {
            return Value::Null;
        }
        match value {
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .filter(|(key, _)| {
                        let key = key.to_ascii_lowercase().replace(['_', '-'], "");
                        !key.contains("token")
                            && !key.contains("password")
                            && !key.contains("secret")
                            && !key.contains("authorization")
                            && !key.contains("credential")
                            && !key.contains("dpop")
                            && !key.contains("appkey")
                            && !key.contains("accesskey")
                            && !matches!(key.as_str(), "modelauthinfo" | "privatekeyjwk" | "apikey")
                    })
                    .map(|(key, value)| (key.clone(), scrub(value, depth + 1)))
                    .collect(),
            ),
            Value::Array(values) => {
                Value::Array(values.iter().map(|value| scrub(value, depth + 1)).collect())
            }
            _ => value.clone(),
        }
    }
    fn hide(value: &mut Value, needles: &[String]) {
        match value {
            Value::Object(fields) => {
                for value in fields.values_mut() {
                    hide(value, needles);
                }
            }
            Value::Array(values) => {
                for value in values {
                    hide(value, needles);
                }
            }
            Value::String(text) => {
                for needle in needles.iter().filter(|needle| !needle.is_empty()) {
                    *text = text.replace(needle, "[已隐藏]");
                }
            }
            _ => {}
        }
    }
    let credential = Credentials::from_secret(secret);
    let mut needles: Vec<String> = credential
        .secrets()
        .into_iter()
        .map(str::to_string)
        .collect();
    needles.push(super::util::text(
        secret,
        &["refreshToken", "refresh_token"],
    ));
    for key in ["private_key_jwk", "privateKeyJwk"] {
        needles.push(super::util::text(&secret["dpopKeyPair"][key], &["d"]));
    }
    let mut output = scrub(payload, 0);
    hide(&mut output, &needles);
    output
}

pub fn control_scope(mut error: Error) -> Error {
    error.credential_scope = Some("subscription".into());
    error
}

pub fn permission(error: &Error) -> bool {
    error.code.as_deref().is_some_and(|code| {
        matches!(
            code,
            "forbidden" | "unsupported" | "no_method" | "bad_params"
        ) || code.contains("origin")
            || code.contains("proxy")
            || code.contains("permission")
    })
}

pub fn uncertain(method: &str, error: Error) -> Error {
    if method == "POST" && !permission(&error) {
        control_scope(error.with_code(UNCERTAIN))
    } else {
        error
    }
}

pub fn control_response(status: u16, value: &Value, credential: &Credentials) -> Option<Error> {
    let code = super::util::text(value, &["error_code", "code"]);
    let detail = super::util::text(
        value,
        &["error_msg", "error_message", "failed_reason", "message"],
    );
    let has_error = value.get("error").is_some_and(|error| !error.is_null());
    let numeric_code = value
        .get("error_code")
        .or_else(|| value.get("code"))
        .and_then(super::util::number);
    let accepted_code = matches!(code.as_str(), "" | "0" | "200")
        && numeric_code.is_none_or(|code| matches!(code, 0.0 | 200.0));
    if (200..300).contains(&status)
        && accepted_code
        && !has_error
        && value["success"] != false
        && value["status"] != "FAILED"
    {
        return None;
    }
    let lower = detail.to_ascii_lowercase();
    let expired = code == "APIG.0307"
        || matches!(code.as_str(), "APIG.0602" | "APIG.1002" | "APIG.0301")
            && [
                "security token has expired",
                "access token is expired",
                "token expires",
                "token has expired",
                "token expired",
            ]
            .iter()
            .any(|part| lower.contains(part));
    let reason = if expired {
        "临时凭据已过期，需要刷新登录授权"
    } else if status == 403 {
        "拒绝访问，请核对套餐、权限或实名认证；不能据此认定凭据过期"
    } else if status == 401 {
        "身份或签名校验失败；不能据此认定凭据过期"
    } else {
        "接口拒绝了请求"
    };
    let message = clean(
        &format!("OfficeAce {reason}（HTTP {status}）：{code} {detail}"),
        &credential.secrets(),
    );
    let status = if expired {
        401
    } else if (400..600).contains(&status) {
        status
    } else {
        502
    };
    let mut error = Error::http(status, message);
    if expired {
        error = error.with_code(CONTROL_EXPIRED);
    }
    Some(control_scope(error))
}

pub fn classify(status: u16, body: &Value) -> Value {
    let text = body.to_string();
    let lower = text.to_ascii_lowercase();
    let detail: String = text.chars().take(2000).collect();
    if [
        "81113",
        "exceeds the maximum size",
        "requestentitytoolarge",
        "payload too large",
        "request body is too large",
        "request body is too long",
    ]
    .iter()
    .any(|part| lower.contains(part))
    {
        return json!({ "kind": "fatal", "status": 413, "upstream_code": upstream_code(body),
            "message": format!("请求超过上游 messages 条数限制（实测5000条），请减少历史消息条数：{detail}") });
    }
    if body["providerCode"] == "officeace_content_filter"
        || body.pointer("/error/code").and_then(Value::as_str) == Some("officeace_content_filter")
    {
        return json!({ "kind": "content_blocked", "status": 400, "upstream_code": null, "message": "OfficeAce 内容审核拒绝了请求，请调整内容后重试" });
    }
    let status = if (400..600).contains(&status) {
        status
    } else {
        502
    };
    let mut limited = status == 429
        || [
            "81111",
            "81112",
            "81114",
            "0308",
            "tpm",
            "rate limit",
            "too many requests",
        ]
        .iter()
        .any(|part| lower.contains(part));
    if upstream_code(body).is_some_and(|code| [81111, 81112, 81114, 308].contains(&code)) {
        limited = true;
    }
    json!({ "kind": if limited { "quota_limited" } else { "fatal" }, "status": status,
        "message": format!("上游返回 {status}：{detail}"), "upstream_code": upstream_code(body), "reset_at": null })
}

fn upstream_code(body: &Value) -> Option<i64> {
    ["/error/code", "/error_code", "/code"]
        .iter()
        .find_map(|path| {
            let value = body.pointer(path)?;
            value.as_i64().filter(|code| *code > 0).or_else(|| {
                value
                    .as_str()?
                    .rsplit('.')
                    .next()?
                    .parse::<i64>()
                    .ok()
                    .filter(|code| *code > 0)
            })
        })
}

pub fn model_error(status: u16, value: &Value, credential: &Credentials) -> Error {
    let class = classify(status, value);
    let status = class["status"].as_u64().unwrap_or(502) as u16;
    let mut secrets = credential.secrets().to_vec();
    let basic = super::client::basic(credential);
    secrets.push(&basic);
    let message = clean(
        class["message"]
            .as_str()
            .unwrap_or("OfficeAce 模型请求失败"),
        &secrets,
    );
    let mut error = Error::http(status, message).with_code(if class["kind"] == "content_blocked" {
        "officeace_content_filter"
    } else {
        "officeace_model_error"
    });
    error.upstream_code = upstream_code(value);
    error.credential_scope = Some("model".into());
    error
}
