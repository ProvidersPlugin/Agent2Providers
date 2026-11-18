//! 余额不足优先于会话失效和速率限制。只对错误体匹配关键词，不扫描正常回答。
//! 返回固定提示及结构化业务码，不把可能夹带凭证的上游原文输出到日志或界面。

use agent2api_wasm_sdk::{json, Error, Value};

const CREDIT: &[&str] = &[
    "insufficient credit",
    "no credit",
    "credit exhausted",
    "out of credit",
    "quota exceeded",
    "quota exhaust",
    "payment required",
    "credit not enough",
    "not enough credit",
    "freecreditsused",
    "free credits used",
    "free quota",
    "quota used up",
    "upgrade your plan",
    "upgrade to continue",
    "积分不足",
    "额度不足",
    "余额不足",
    "积分用完",
    "额度用尽",
    "没有积分",
    "积分耗尽",
    "额度已用完",
    "升级套餐",
];
const SESSION: &[&str] = &[
    "40100",
    "40101",
    "token rejected",
    "refresh token was rejected",
    "lobsterai_session",
];
const CONTENT: &[&str] = &[
    "blocked by security policy",
    "unapproved channel",
    "illegal api invocation",
    "content_filter",
    "safety_violation",
];

fn hit(text: &str, markers: &[&str]) -> bool {
    markers.iter().any(|marker| text.contains(marker))
}

pub fn code(value: &Value) -> Option<i64> {
    for field in [
        &value["code"],
        &value["upstreamCode"],
        &value["upstream_code"],
        &value["error"]["code"],
    ] {
        if let Some(code) = field
            .as_i64()
            .or_else(|| field.as_str().and_then(|text| text.parse().ok()))
        {
            return Some(code);
        }
    }
    None
}

pub fn classify(status: u16, body: &Value) -> Value {
    let text = body.to_string().to_lowercase();
    let (kind, status, message) = if status == 402
        || hit(&text, CREDIT)
        || text.contains("lobsterai_credit")
    {
        (
            "quota_limited",
            402,
            "LobsterAI 积分不足，请签到补充积分或切换账号",
        )
    } else if hit(&text, SESSION) || code(body).is_some_and(|code| matches!(code, 40100 | 40101)) {
        ("token_expired", 401, "LobsterAI 登录凭证已失效，请重新登录")
    } else if status == 429
        || code(body) == Some(42900)
        || hit(
            &text,
            &[
                "rate limit",
                "ratelimit",
                "too many",
                "限流",
                "频率",
                "过于频繁",
                "过快",
            ],
        )
    {
        ("quota_limited", 429, "LobsterAI 请求过于频繁，请稍后再试")
    } else if hit(&text, CONTENT) || text.contains("lobsterai_content") {
        (
            "content_blocked",
            403,
            "LobsterAI 内容审核未通过，请调整提示内容",
        )
    } else if status == 403 || code(body) == Some(40300) {
        ("fatal", 403, "LobsterAI 未授权该模型或操作，请检查账号资格")
    } else if status >= 500 {
        ("fatal", status.min(599), "LobsterAI 上游服务暂时不可用")
    } else if status == 401 {
        ("token_expired", 401, "LobsterAI 登录凭证已失效，请重新登录")
    } else if status == 404 {
        ("fatal", 404, "LobsterAI 请求的模型或接口不存在")
    } else {
        (
            "fatal",
            status.clamp(400, 599),
            "LobsterAI 未接受本次请求，请检查模型和请求参数",
        )
    };
    json!({ "kind": kind, "status": status, "message": message,
        "upstream_code": code(body), "reset_at": reset_at(body) })
}

pub fn upstream(status: u16, body: &Value) -> Error {
    let class = classify(status, body);
    let code = match class["kind"].as_str() {
        Some("token_expired") => "lobsterai_session",
        Some("content_blocked") => "lobsterai_content",
        Some("quota_limited") if class["status"] == 402 => "lobsterai_credit",
        Some("quota_limited") => "lobsterai_rate",
        _ => "lobsterai_upstream",
    };
    let mut error = Error::http(
        class["status"].as_u64().unwrap_or(502) as u16,
        class["message"]
            .as_str()
            .unwrap_or("LobsterAI 上游调用失败"),
    )
    .with_code(code);
    error.upstream_code = class["upstream_code"].as_i64();
    error.reset_at = class["reset_at"].as_i64();
    error
}

pub fn session(message: &str) -> Error {
    Error::http(401, message).with_code("lobsterai_session")
}

fn reset_at(body: &Value) -> Option<i64> {
    [
        &body["reset_at"],
        &body["resetAt"],
        &body["error"]["reset_at"],
        &body["error"]["resetAt"],
    ]
    .iter()
    .find_map(|value| value.as_i64().filter(|value| *value > 0))
}

pub fn stream_error(data: &str) -> Error {
    let value = serde_json::from_str::<Value>(data).unwrap_or_else(|_| json!({ "message": data }));
    upstream(400, &value)
}

pub fn event(value: &Value) -> Option<Error> {
    if value.get("error").is_some_and(|error| !error.is_null()) || value["type"] == "error" {
        let status = value["error"]["status"]
            .as_u64()
            .or_else(|| value["status"].as_u64())
            .or_else(|| value["error"]["code"].as_u64())
            .filter(|status| (400..600).contains(status))
            .unwrap_or(400) as u16;
        return Some(upstream(status, value));
    }
    if let Some(code) = code(value).filter(|code| *code != 0 && *code != 200) {
        return Some(upstream(
            if (400..600).contains(&code) {
                code as u16
            } else {
                400
            },
            value,
        ));
    }
    if value.get("code").is_some()
        && code(value) == Some(0)
        && value["data"].is_null()
        && value["choices"].is_null()
    {
        return Some(session("LobsterAI 响应未返回数据，登录状态可能已失效"));
    }
    None
}

pub fn stream_event(error: &Error) -> Value {
    json!({ "error": { "message": error.message, "status": error.status.unwrap_or(502),
        "code": error.code, "type": "upstream_error", "upstream_code": error.upstream_code,
        "reset_at": error.reset_at, "credential_scope": error.credential_scope } })
}
