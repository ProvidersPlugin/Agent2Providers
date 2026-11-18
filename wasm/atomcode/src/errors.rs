use agent2api_wasm_sdk::{json, Error, Value};

fn integer(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str().and_then(|raw| raw.parse().ok()))
}

pub fn status(body: &Value) -> Option<u16> {
    ["status", "error_code", "code"].iter().filter_map(|key| integer(&body[*key]))
        .chain([&body["error"], &body["detail"]].into_iter().flat_map(|value| {
            ["status", "code"].into_iter().filter_map(move |key| integer(&value[key]))
        }))
        .filter_map(|value| u16::try_from(value).ok()).find(|status| (400..600).contains(status))
}

pub fn business_error(body: &Value) -> bool {
    body.get("error").is_some_and(|value| !value.is_null() && value != false)
        || body.get("detail").is_some_and(|value| value.is_object()
            && (value.get("code").is_some() || value.get("error").is_some()))
        || ["error_code", "code"].iter().any(|key| integer(&body[*key])
            .is_some_and(|code| code != 0 && !(200..300).contains(&code)))
        || body["success"] == false
}

pub fn classify(status: u16, body: &Value) -> Value {
    let raw = body.to_string();
    let message = [&body["message"], &body["error_message"], &body["error"]["message"], &body["detail"]["message"]]
        .into_iter().find_map(Value::as_str);
    let parameter_rejected = message.is_some_and(|text| text.trim() == "参数错误");
    let lower = raw.to_ascii_lowercase();
    let status = self::status(body).unwrap_or(status);
    let (kind, status, message) = if ["ATOMCODE_SIG_", "ATOMCODE_UA_", "ATOMCODE_VER_"]
        .iter().any(|prefix| raw.contains(prefix)) || raw.contains("atomcode_client_rejected") {
        ("fatal", 502, "AtomCode 上游拒绝了客户端签名或版本，请检查插件与官方网关配置")
    } else if lower.contains("access disabled by request policy") || raw.contains("atomcode_request_policy") {
        ("fatal", 403, "AtomCode 当前模型被上游请求策略拒绝，请改用账号目录中的其他模型；这不是登录凭证过期")
    } else if raw.contains("atomcode_parameter_rejected") || parameter_rejected {
        ("fatal", 502, "AtomCode 返回成功状态但拒绝了模型或请求参数")
    } else if ["content_filter", "content_policy", "safety_violation"].iter().any(|marker| lower.contains(marker)) {
        ("content_blocked", 403, "AtomCode 上游内容审核未通过")
    } else if matches!(status, 401 | 403) && lower.contains("codingplan") {
        ("fatal", 403, "AtomGit 账号没有有效 CodingPlan，请先领取套餐")
    } else if matches!(status, 401 | 403) || lower.contains("atomcode_refresh_expired") {
        ("token_expired", 401, "AtomCode 登录凭证已失效")
    } else if status == 429 || raw.contains("在当前 CodingPlan 用量窗口内已达上限") {
        ("quota_limited", 429, "AtomCode 当前用量窗口已达上限")
    } else {
        ("fatal", status.max(400), "AtomCode 上游请求未成功")
    };
    let reset_at = body["reset_at"].as_i64().or_else(|| body["resetAt"].as_i64())
        .or_else(|| body["error"]["reset_at"].as_i64())
        .or_else(|| body["error"]["resetAt"].as_i64())
        .or_else(|| body["detail"]["reset_at"].as_i64())
        .or_else(|| quota_reset_at(&raw));
    let upstream_code = body["upstream_code"].as_i64()
        .or_else(|| body["error"]["upstream_code"].as_i64())
        .or_else(|| integer(&body["error_code"]))
        .or_else(|| integer(&body["code"]))
        .or_else(|| integer(&body["error"]["code"]))
        .or_else(|| integer(&body["detail"]["code"]));
    json!({ "kind": kind, "status": status, "message": message,
        "reset_at": reset_at, "upstream_code": upstream_code })
}

pub fn http_error(status: u16, body: &Value) -> Error {
    let class = classify(status, body);
    let raw = body.to_string();
    let code = if ["ATOMCODE_SIG_", "ATOMCODE_UA_", "ATOMCODE_VER_"].iter().any(|prefix| raw.contains(prefix))
        || raw.contains("atomcode_client_rejected") {
        "atomcode_client_rejected"
    } else if raw.to_ascii_lowercase().contains("access disabled by request policy") || raw.contains("atomcode_request_policy") {
        "atomcode_request_policy"
    } else if raw.to_ascii_lowercase().contains("codingplan") && class["kind"] == "fatal" {
        "codingplan"
    } else if raw.contains("atomcode_parameter_rejected")
        || body["message"].as_str().is_some_and(|text| text.trim() == "参数错误") { "atomcode_parameter_rejected" }
    else if class["kind"] == "content_blocked" { "content_filter" }
    else if class["kind"] == "token_expired" { "atomcode_refresh_expired" }
    else { "atomcode_upstream" };
    let mut error = Error::http(class["status"].as_u64().unwrap_or(502) as u16,
        class["message"].as_str().unwrap_or("AtomCode 上游错误"))
        .with_code(code).with_reset(class["reset_at"].as_i64());
    error.upstream_code = class["upstream_code"].as_i64();
    if code == "atomcode_refresh_expired" { error.credential_scope = Some("access".to_string()); }
    error
}

pub fn silent(text: &str) -> Option<Error> {
    let text = text.trim();
    if text == "参数错误" {
        return Some(Error::http(502, "AtomCode 返回成功状态但拒绝了模型或请求参数")
            .with_code("atomcode_parameter_rejected"));
    }
    if text.starts_with("三方请求失败") && (text.to_ascii_lowercase().contains("<html")
        || text.to_ascii_lowercase().contains("<!doctype")
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
    json!({ "error": { "message": error.message, "status": error.status.unwrap_or(502),
        "code": error.code, "upstream_code": error.upstream_code, "reset_at": error.reset_at,
        "credential_scope": error.credential_scope, "type": "upstream_error" } })
}
