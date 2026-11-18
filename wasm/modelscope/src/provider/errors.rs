//! 原生错误判据原样保留：魔粒耗尽/邮箱未验证先于裸 401、403、429。
//! model_not_ready 是模型后端状态而非账号额度，仍按 fatal，不惩罚账号。

use agent2api_wasm_sdk::{self as sdk, json, Error, Value};

use super::{clock, Region};

pub fn status(params: &Value) -> u16 {
    ["status", "status_code", "code"]
        .iter()
        .find_map(|key| number(&params[*key]))
        .filter(|status| (100..600).contains(status))
        .map(|status| status as u16)
        .unwrap_or(502)
}

fn number(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str()?.trim().parse().ok())
}

pub fn message(body: &Value) -> String {
    for value in [body.get("error"), Some(body)].into_iter().flatten() {
        if let Some(raw) = value.as_str().map(str::trim).filter(|raw| !raw.is_empty()) {
            return raw.chars().take(3000).collect();
        }
        for key in ["message", "Message", "msg"] {
            if let Some(raw) = value[key]
                .as_str()
                .map(str::trim)
                .filter(|raw| !raw.is_empty())
            {
                return raw.chars().take(3000).collect();
            }
        }
    }
    "上游错误".to_string()
}

fn upstream_code(body: &Value) -> Option<i64> {
    [body.get("error"), Some(body)]
        .into_iter()
        .flatten()
        .find_map(|value| {
            value
                .get("code")
                .or_else(|| value.get("Code"))
                .and_then(number)
        })
}

pub fn classify(region: Region, status: u16, body: &Value) -> Value {
    let raw = message(body);
    let lower = raw.to_ascii_lowercase();
    let code = upstream_code(body);
    let message = format!("上游返回 {status}: {raw}");
    let label = region.label();
    let next_day = || Some(clock::next_day_boundary(sdk::now()));
    let (kind, message, reset_at) = if status == 429 && lower.contains("insufficient balance") {
        ("quota_limited", format!("ModelScope {label}账号的魔粒已耗尽（上游：{raw}），将在下一个自然日（北京时间 0 点）自动恢复；给账号充值后立即恢复"), next_day())
    } else if status == 403
        && [
            "email verification",
            "verified accounts",
            "/my/settings/account",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        ("quota_limited", format!("ModelScope {label}账号未完成邮箱验证，魔粒推理接口拒绝服务（上游：{raw}）——请到魔搭账号设置页完成邮箱验证，验证后次日自动恢复"), next_day())
    } else if [
        "blocked by security policy",
        "unapproved channel",
        "illegal api invocation",
        "content_filter",
        "content blocked",
        "content policy",
        "内容审核",
        "内容安全",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        (
            "content_blocked",
            format!("{message}；内容被上游内容策略拒绝，这不是账号问题；请调整内容后重试"),
            None,
        )
    } else if matches!(status, 401 | 403) {
        ("token_expired", message, None)
    } else if status == 429 {
        ("quota_limited", message, None)
    } else {
        ("fatal", message, None)
    };
    json!({ "kind": kind, "status": status, "message": message,
        "upstream_code": code, "reset_at": reset_at })
}

pub fn http_error(region: Region, status: u16, body: &Value) -> Error {
    let status = if (400..600).contains(&status) {
        status
    } else {
        502
    };
    let class = classify(region, status, body);
    // 错误中保留原始特征文案，宿主在首帧前重新分类时仍能区分两类 429/403。
    let mut error = Error::http(
        status,
        class["message"].as_str().unwrap_or("ModelScope 上游错误"),
    )
    .with_code("modelscope_upstream")
    .with_reset(class["reset_at"].as_i64());
    error.upstream_code = class["upstream_code"].as_i64();
    error
}

pub fn from_bytes(region: Region, status: u16, bytes: &[u8]) -> Error {
    let body = serde_json::from_slice(bytes)
        .unwrap_or_else(|_| json!({ "message": String::from_utf8_lossy(bytes) }));
    http_error(region, status, &body)
}

pub fn event(error: &Error) -> Value {
    json!({ "error": { "message": error.message, "status": error.status.unwrap_or(502),
        "type": "upstream_error", "code": error.code, "upstream_code": error.upstream_code,
        "reset_at": error.reset_at, "credential_scope": error.credential_scope } })
}
