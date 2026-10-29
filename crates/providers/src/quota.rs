//! 提供商声明的额度来源；字段独立于余额展示文案，宿主不按提供商猜计费口径。

mod zcode;

use serde_json::{json, Value};

pub fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }.filter(|number| number.is_finite())
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

pub fn source(provider: &str, id: &str, label: &str, unit: &str, remaining: Option<f64>, total: Option<f64>, used: Option<f64>) -> Value {
    json!({ "id": id, "label": label, "provider": provider, "unit": unit,
        "remaining": remaining, "total": total, "used": used })
}

pub fn sources(provider: &str, report: &Value) -> Vec<Value> {
    if let Some(sources) = report.get("quotaSources").and_then(Value::as_array) { return sources.clone(); }
    let mut sources = Vec::new();
    let unit = text(report, "unit");
    let remaining = number(report.get("totalLeft")).or_else(|| number(report.get("available")));
    if remaining.is_some() || report.get("unlimited").and_then(Value::as_bool) == Some(true) {
        let mut account = source(provider, "account", "账号主额度", unit, remaining, None, None);
        account["unlimited"] = report.get("unlimited").cloned().unwrap_or(json!(false));
        sources.push(account);
    }
    match provider {
        "zcode" | "zcode-intl" => zcode::append(provider, report, &mut sources),
        "codearts" => codearts(provider, report, &mut sources),
        "antigravity" => windows(provider, report, &mut sources),
        "atomcode" => atomcode(provider, report, &mut sources),
        "accio" | "accio-cn" => accio(provider, report, &mut sources),
        "qoder" | "qoder-intl" => credits(provider, report, unit, &mut sources),
        "workbuddy" | "workbuddy-intl" => credits(provider, report, "积分", &mut sources),
        "trae" | "officeace" | "lobsterai" => credits(provider, report, unit, &mut sources),
        "loomy" | "raccoon" | "autoclaw" | "autoclaw-intl" => wallets(provider, report, unit, &mut sources),
        _ => {},
    }
    sources
}

fn credits(provider: &str, report: &Value, unit: &str, output: &mut Vec<Value>) {
    for row in report.get("credits").and_then(Value::as_array).into_iter().flatten() {
        if row.get("record").and_then(Value::as_bool) == Some(true) { continue; }
        let name = text(row, "name");
        let remaining = number(row.get("remain"));
        let total = number(row.get("total"));
        let used = number(row.get("used"));
        if remaining.is_none() && total.is_none() && used.is_none() { continue; }
        let row_unit = if text(row, "unit").is_empty() { unit } else { text(row, "unit") };
        let explicit = text(row, "id");
        let daily = row.get("daily").and_then(Value::as_bool) == Some(true);
        let id = if !explicit.is_empty() { format!("credit:{explicit}") }
            else if daily { format!("credit:{name}:daily") }
            else { format!("credit:{name}:{}:{}", row.get("grantAt").unwrap_or(&Value::Null), row.get("expireAt").unwrap_or(&Value::Null)) };
        let mut source = source(provider, &id, name, row_unit, remaining, total, used);
        source["unlimited"] = row.get("unlimited").cloned().unwrap_or(json!(false));
        source["startsAt"] = json!(number(row.get("startAt")).unwrap_or(0.0) as i64);
        source["endsAt"] = json!(number(row.get("expireAt")).unwrap_or(0.0) as i64);
        source["planId"] = json!(id);
        source["planName"] = json!(name);
        if daily {
            source["endsAt"] = json!(0);
            source["resetsAt"] = json!(number(row.get("expireAt")).unwrap_or(0.0) as i64);
            source["window"] = json!({ "reset": "daily", "utcOffsetMinutes": 480 });
        }
        output.push(source);
    }
}

fn wallets(provider: &str, report: &Value, unit: &str, output: &mut Vec<Value>) {
    for row in report.get("wallets").and_then(Value::as_array).into_iter().flatten() {
        let kind = text(row, "type");
        let remaining = number(row.get("balance"));
        if kind.is_empty() || remaining.is_none() { continue; }
        let mut source = source(provider, &format!("wallet:{kind}"), text(row, "displayName"), unit,
            remaining, number(row.get("limit")), number(row.get("used")));
        if let Some(window) = row.get("window").filter(|value| value.is_object()) {
            source["window"] = window.clone();
        }
        output.push(source);
    }
}

fn codearts(provider: &str, report: &Value, output: &mut Vec<Value>) {
    for row in report.get("wallets").and_then(Value::as_array).into_iter().flatten() {
        let kind = text(row, "type");
        if !kind.starts_with("meter:") { continue; }
        let source = source(provider, kind, text(row, "displayName"), "积分",
            number(row.get("creditRemaining")), number(row.get("creditTotal")), number(row.get("creditUsed")));
        if number(source.get("remaining")).is_some() || number(source.get("used")).is_some() { output.push(source); }
    }
}

fn windows(provider: &str, report: &Value, output: &mut Vec<Value>) {
    for row in report.get("windows").and_then(Value::as_array).into_iter().flatten() {
        let mut source = source(provider, &format!("window:{}", text(row, "key")), text(row, "label"), "%", None, None, None);
        source["remainingPercent"] = row.get("remainingPercent").cloned().unwrap_or(Value::Null);
        source["resetsAt"] = row.get("resetsAt").cloned().unwrap_or(json!(0));
        source["models"] = row.get("models").cloned().unwrap_or(json!([]));
        output.push(source);
    }
}

fn atomcode(provider: &str, report: &Value, output: &mut Vec<Value>) {
    for row in report.get("wallets").and_then(Value::as_array).into_iter().flatten() {
        let hours = number(row.get("windowHours")).unwrap_or(0.0);
        let mut source = source(provider, &format!("calls:{hours}"), text(row, "displayName"), "次",
            number(row.get("remaining")), number(row.get("limit")), number(row.get("used")));
        if hours > 0.0 { source["window"] = json!({ "reset": "rolling", "seconds": (hours * 3600.0) as i64 }); }
        output.push(source);
    }
}

fn accio(provider: &str, report: &Value, output: &mut Vec<Value>) {
    for row in report.get("wallets").and_then(Value::as_array).into_iter().flatten() {
        let mut source = source(provider, "entitlement", text(row, "displayName"), "%", None, None, None);
        source["remainingPercent"] = row.get("balance").cloned().unwrap_or(Value::Null);
        let seconds = number(report.pointer("/raw/quota/refreshCountdownSeconds"))
            .or_else(|| number(report.pointer("/raw/quota/refresh_countdown_seconds")));
        if let Some(seconds) = seconds { source["resetsAt"] = json!(crate::logging::now_ms() + (seconds * 1000.0) as i64); }
        output.push(source);
    }
}
