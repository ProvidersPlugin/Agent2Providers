use super::network::{integer, text};
use agent2api_wasm_sdk::{self as sdk, json, Error, Value};
use chrono::TimeZone;

pub fn code(body: &Value) -> Option<i64> {
    [
        body.get("upstreamCode"),
        body.get("upstream_code"),
        body.get("code"),
        body.pointer("/error/upstreamCode"),
        body.pointer("/error/upstream_code"),
        body.pointer("/error/code"),
    ]
    .into_iter()
    .flatten()
    .find_map(integer)
}
pub fn has_error(body: &Value) -> bool {
    body["type"] == "error"
        || body.get("error").is_some_and(|error| !error.is_null())
        || body["success"] == false
        || code(body).is_some_and(|code| !matches!(code, 0 | 200))
}
fn message(body: &Value) -> String {
    let value = text(body, &["message", "msg"]);
    if value.is_empty() {
        let nested = text(&body["error"], &["message", "msg"]);
        if nested.is_empty() {
            body.as_str().unwrap_or("").to_string()
        } else {
            nested
        }
    } else {
        value
    }
}
pub fn risk(status: u16, body: &Value) -> bool {
    match code(body) {
        Some(code) => code == 3012,
        None => {
            matches!(status, 403 | 405)
                && message(body)
                    .to_ascii_lowercase()
                    .contains("request has been blocked due to unusual activity")
        }
    }
}
fn temporary(body: &Value) -> bool {
    matches!(code(body), Some(1302 | 1305 | 3008 | 3009 | 3010))
}
pub fn stop(status: u16, body: &Value) -> bool {
    risk(status, body)
        || matches!(
            code(body),
            Some(3001 | 3007 | 1309 | 1311 | 1313 | 1314 | 1315)
        )
}
fn diagnosed(body: &Value) -> bool {
    [body["providerCode"].as_str(), body["code"].as_str()]
        .into_iter()
        .flatten()
        .any(|code| {
            matches!(
                code,
                "zcode_start_plan_unavailable"
                    | "zcode_start_plan_pending"
                    | "zcode_start_model_unavailable"
                    | "zcode_start_model_exhausted"
            )
        })
}
fn reset_at(body: &Value) -> Option<i64> {
    let now = sdk::now();
    let explicit = [
        body.get("resetAt"),
        body.get("reset_at"),
        body.pointer("/error/resetAt"),
    ]
    .into_iter()
    .flatten()
    .find_map(integer)
    .and_then(|at| {
        if at > 0 && at < 100_000_000_000 {
            at.checked_mul(1000)
        } else {
            Some(at)
        }
    });
    if let Some(at) = explicit.filter(|at| {
        *at > now
            && *at <= (1_i64 << 53) - 1
            && chrono::DateTime::from_timestamp_millis(*at).is_some()
    }) {
        return Some(at);
    }
    if !matches!(code(body), Some(1308 | 1310)) {
        return None;
    }
    let raw = message(body);
    if !raw.contains("重置") && !raw.to_ascii_lowercase().contains("reset") {
        return None;
    }
    let zone = chrono::FixedOffset::east_opt(8 * 3600)?;
    raw.as_bytes()
        .windows(19)
        .filter_map(|bytes| std::str::from_utf8(bytes).ok())
        .filter_map(|text| {
            chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S")
                .ok()
                .or_else(|| chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S").ok())
        })
        .filter_map(|time| {
            zone.from_local_datetime(&time)
                .single()
                .map(|time| time.timestamp_millis())
        })
        .find(|at| *at > now)
}

pub fn version_refused(status: u16, body: &Value) -> bool {
    if !(200..300).contains(&status) {
        return false;
    }
    let Some(map) = body.as_object() else {
        return false;
    };
    if body["success"] == false || code(body) != Some(0) || !message(body).trim().is_empty() {
        return false;
    }
    !["data", "error", "content", "choices", "result", "usage", "parts", "candidates"]
        .iter()
        .any(|key| map.get(*key).is_some_and(|value| !value.is_null()))
}

pub fn version_refused_error(version: Option<&str>) -> Error {
    let message = match version.filter(|value| !value.trim().is_empty()) {
        Some(version) => format!(
            "ZCode 上游返回空响应（200 code=0），当前请求的客户端版本（{version}）可能已不被上游接受；请在提供商设置中同步最新 ZCode 版本或更新插件后重试"
        ),
        None => "ZCode 上游返回空响应（200 code=0），当前请求的客户端版本可能已不被上游接受；请在提供商设置中同步最新 ZCode 版本或更新插件后重试".to_string(),
    };
    Error::http(400, message).with_code("zcode_version_refused")
}

pub fn classify(status: u16, body: &Value) -> Value {
    let code = code(body);
    let raw = message(body);
    let lower = raw.to_ascii_lowercase();
    let blocked = body["code"] == "content_filter"
        || code == Some(1301)
        || [
            "blocked by security policy",
            "unapproved channel",
            "illegal api invocation",
        ]
        .iter()
        .any(|pattern| lower.contains(*pattern));
    let failed = if (400..600).contains(&status) {
        status
    } else {
        502
    };
    let (kind, status, description) = if risk(status, body) {
        (
            "fatal",
            failed,
            "ZCode 上游风控拒绝（3012），不是额度耗尽；已停止本次重试，请到官方客户端核对",
        )
    } else if code == Some(3007) {
        (
            "fatal",
            if status == 429 || status < 400 {
                428
            } else {
                failed
            },
            "ZCode 上游要求新的官方人机验证；这不是账号失效或额度不足",
        )
    } else if status == 401 || matches!(code, Some(401 | 1000 | 1001 | 1002 | 1003 | 1004)) {
        (
            "token_expired",
            401,
            "ZCode 当前套餐通道的凭据已被拒绝，请重新登录",
        )
    } else if code == Some(3001) {
        (
            "fatal",
            failed,
            "ZCode 上游参数校验失败，请核对设备标识与请求格式",
        )
    } else if blocked {
        (
            "content_blocked",
            failed,
            "ZCode 上游内容策略拒绝，请调整系统提示或对话内容后重试",
        )
    } else if temporary(body) {
        (
            "fatal",
            429,
            match code {
                Some(3008) => {
                    "ZCode 账号当前并发请求已满（3008），请等待正在运行的请求结束；不是额度用完"
                }
                Some(3009) => {
                    "ZCode 当前模型并发请求已满（3009），请等待正在运行的请求结束；不是额度用完"
                }
                Some(3010) => "ZCode 套餐池当前请求暂时繁忙（3010），请稍后重试；未判定额度用完",
                Some(1302) => {
                    "ZCode 账号触发短期速率限制（1302），请降低并发或请求频率；不是套餐额度用完"
                }
                _ => "ZCode 上游模型暂时繁忙（1305），请稍后重试；不是账号额度用完",
            },
        )
    } else if matches!(code, Some(1309 | 1311 | 1313 | 1314 | 1315)) {
        (
            "fatal",
            429,
            match code {
                Some(1309) => {
                    "ZCode 编码套餐已到期（1309），请在官方页面核对订阅；未切换或购买套餐"
                }
                Some(1311) => "ZCode 当前订阅未开放所选模型权限（1311），不是余额不足",
                Some(1313) => {
                    "ZCode 上游公平使用策略限制（1313），请到官方页面核对；已停止重复请求"
                }
                Some(1314) => "ZCode 企业套餐已失效（1314），请联系企业管理员",
                _ => "ZCode 当前套餐无所选模型的使用权限（1315），请核对订阅与模型",
            },
        )
    } else if matches!(code, Some(1005 | 1113 | 1308 | 1310)) {
        ("quota_limited", 429, match code {
            Some(1005) if diagnosed(body) => raw.as_str(),
            Some(1005) => "ZCode 套餐池当前模型没有可用额度（1005）：套餐可能未生效、已到期或模型额度已用完；未切换到付费通道",
            Some(1113) => "ZCode 当前通道没有可用余额或资源包（1113），请核对套餐通道、密钥及有效期；未自动切换或充值",
            Some(1308) => "ZCode 编码套餐当前使用窗口已达到上限（1308），请等待官方重置时间",
            _ => "ZCode 编码套餐每周或每月额度已达到上限（1310），请等待官方重置时间",
        })
    } else if status == 429 {
        ("fatal", 429, "ZCode 上游返回 429，但没有明确的额度耗尽业务码；未把账号标记为额度用完，请核对上游限制原因")
    } else if version_refused(status, body) {
        (
            "fatal",
            400,
            "ZCode 上游返回空响应（200 code=0），当前请求的客户端版本可能已不被上游接受；请在提供商设置中同步最新 ZCode 版本或更新插件后重试",
        )
    } else {
        ("fatal", failed, "ZCode 上游请求未成功")
    };
    json!({ "kind": kind, "status": status, "message": description, "upstream_code": code, "reset_at": reset_at(body) })
}

pub fn failure(status: u16, body: &Value) -> Error {
    if version_refused(status, body) {
        return version_refused_error(None);
    }
    let classified = classify(status, body);
    let code = code(body);
    let mut error = Error::http(
        classified["status"].as_u64().unwrap_or(502) as u16,
        classified["message"]
            .as_str()
            .unwrap_or("ZCode 上游请求失败"),
    );
    error.code = Some(if classified["kind"] == "content_blocked" {
        "content_filter".into()
    } else {
        code.map(|value| value.to_string())
            .unwrap_or_else(|| "upstream_error".into())
    });
    error.upstream_code = code;
    error.reset_at = classified["reset_at"].as_i64();
    error
}

pub fn retry(_params: &Value) -> Value {
    json!({ "action": "default" })
}

pub fn is_error_response(params: &Value) -> bool {
    let status = params["status"].as_u64().unwrap_or(502);
    !(200..300).contains(&status)
        || params["headers"]["content-type"]
            .as_str()
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("application/json")
            })
}

pub fn event(error: &Error) -> Value {
    json!({ "error": { "message": error.message, "code": error.code, "upstreamCode": error.upstream_code,
        "status": error.status.unwrap_or(502), "type": if error.status == Some(429) { "rate_limit_error" }
        else if error.status == Some(401) { "authentication_error" } else { "upstream_error" } } })
}
