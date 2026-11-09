use agent2api_wasm_sdk::{json, Error, Value};

pub fn classify(status: u16, body: &Value) -> Value {
    let raw = body.to_string();
    let lower = raw.to_ascii_lowercase();
    let (kind, status, message) = if raw.contains("ATOMCODE_SIG_MISSING") {
        ("fatal", 502, "AtomCode 上游要求官方签名，请检查网关地址配置")
    } else if matches!(status, 401 | 403) && lower.contains("codingplan") {
        ("fatal", 403, "AtomGit 账号没有有效 CodingPlan，请先领取套餐")
    } else if ["content_filter", "content_policy", "safety_violation"].iter().any(|marker| lower.contains(marker)) {
        ("content_blocked", 403, "AtomCode 上游内容审核未通过")
    } else if matches!(status, 401 | 403) {
        ("token_expired", 401, "AtomCode 登录凭证已失效")
    } else if status == 429 || raw.contains("在当前 CodingPlan 用量窗口内已达上限") {
        ("quota_limited", 429, "AtomCode 当前用量窗口已达上限")
    } else {
        ("fatal", status.max(400), "AtomCode 上游请求未成功")
    };
    let reset_at = body["resetAt"].as_i64().or_else(|| quota_reset_at(&raw));
    json!({ "kind": kind, "status": status, "message": message, "reset_at": reset_at, "upstream_code": null })
}

pub fn http_error(status: u16, body: &Value) -> Error {
    let class = classify(status, body);
    let code = if body.to_string().contains("ATOMCODE_SIG_MISSING") { "ATOMCODE_SIG_MISSING" }
        else if body.to_string().to_ascii_lowercase().contains("codingplan") && matches!(status, 401 | 403) { "codingplan" }
        else if class["kind"] == "content_blocked" { "content_filter" }
        else { "atomcode_upstream" };
    Error::http(class["status"].as_u64().unwrap_or(502) as u16,
        class["message"].as_str().unwrap_or("AtomCode 上游错误"))
        .with_code(code).with_reset(class["reset_at"].as_i64())
}

pub fn silent(text: &str) -> Option<Error> {
    let text = text.trim();
    if text == "参数错误" {
        return Some(Error::http(502, "AtomCode 返回成功状态但拒绝了模型或请求参数")
            .with_code("atomcode_parameter_rejected"));
    }
    if text.starts_with("三方请求失败") && (text.contains("<html") || text.contains("<!DOCTYPE")
        || text.split([':', '：']).nth(1).is_some_and(|part| part.trim_start().starts_with(['4', '5']))) {
        return Some(Error::http(502, "AtomCode 模型后端返回了错误页面，未作为正常回答转发"));
    }
    if text.starts_with("模型「") && text.contains("在当前 CodingPlan 用量窗口内已达上限") {
        return Some(http_error(429, &json!({ "message": text })));
    }
    None
}

fn quota_reset_at(text: &str) -> Option<i64> {
    use chrono::TimeZone;
    let now = agent2api_wasm_sdk::now();
    let zone = chrono::FixedOffset::east_opt(8 * 3600)?;
    let date = zone.timestamp_millis_opt(now).single()?.date_naive();
    for key in ["当日", "今日"] {
        let Some((_, rest)) = text.split_once(key) else { continue };
        let window: String = rest.chars().take(16).collect();
        let bytes = window.as_bytes();
        for at in 0..bytes.len() {
            if !bytes[at].is_ascii_digit() { continue; }
            let digits = if bytes.get(at + 1).is_some_and(u8::is_ascii_digit) { 2 } else { 1 };
            if bytes.get(at + digits) != Some(&b':') { continue; }
            let Some(end) = bytes.get(at..at + digits + 3) else { continue };
            let Ok(clock) = std::str::from_utf8(end) else { continue };
            let Some((hour, minute)) = clock.split_once(':') else { continue };
            let (Ok(hour), Ok(minute)) = (hour.parse::<u32>(), minute.parse::<u32>()) else { continue };
            let Some(time) = date.and_hms_opt(hour, minute, 0) else { continue };
            let reset = zone.from_local_datetime(&time).single()?.timestamp_millis();
            if reset > now { return Some(reset); }
        }
    }
    None
}

pub fn error_event(error: &Error) -> Value {
    let status = error.status.unwrap_or(502);
    json!({ "error": { "message": error.message, "status": status, "type": "upstream_error" } })
}
