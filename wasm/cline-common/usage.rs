//! credit为微credit/1e6，保留负数；套餐404为无订阅而非余额失败。

use super::{account, client, refresh};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

fn changed() -> Error {
    Error::http(
        409,
        "Cline 余额查询期间账号已变化，请重新查询；未发布旧账号读数",
    )
    .with_code("identity_changed")
}

fn check(record: &Value) -> Result<()> {
    let current = sdk::account()?;
    if ["id", "provider", "identity", "desktop", "secret"]
        .iter()
        .any(|key| current[*key] != record[*key])
        || sdk::account_disabled(&current)
    {
        return Err(changed());
    }
    if account::local(&record["secret"]) {
        let source = super::desktop::read(&record["secret"])?;
        if account::expiry(&source.secret).is_some_and(|expiry| expiry <= sdk::now()) {
            return Err(changed());
        }
    }
    Ok(())
}

pub fn sources(report: &Value) -> Value {
    let mut sources = Vec::new();
    if let Some(remaining) = account::number(&report["available"]) {
        sources.push(json!({ "id": "account", "label": "Cline 按量 credit", "provider": crate::PROVIDER_ID,
            "unit": "credit", "remaining": remaining, "total": null, "used": null, "unlimited": false }));
    }
    if crate::PROVIDER_ID == "cline-pass" {
        for window in report["windows"].as_array().into_iter().flatten() {
            if let (Some(key), Some(percent)) = (
                window["key"].as_str(),
                account::number(&window["remainingPercent"]),
            ) {
                let models = window["models"].as_array().cloned().unwrap_or_default();
                if models.is_empty() {
                    continue;
                }
                sources.push(json!({ "id": key, "label": window["label"], "provider": crate::PROVIDER_ID,
                    "models": models, "unit": "%", "remaining": null, "total": null, "used": null,
                    "remainingPercent": percent, "resetsAt": window["resetsAt"].as_i64().unwrap_or(0), "unlimited": false }));
            }
        }
    }
    json!(sources)
}

fn subscription_windows(data: &Value) -> Result<Vec<Value>> {
    let limits = data["limits"]
        .as_array()
        .filter(|limits| limits.len() <= 32)
        .ok_or_else(|| Error::http(502, "ClinePass 未返回有效的订阅窗口，未当作额度充足"))?;
    let models = super::models::cached()
        .ok()
        .and_then(|catalog| catalog["models"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|model| model["id"].as_str())
        .filter(|model| model.starts_with("cline-pass/"))
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut windows = Vec::new();
    for (kind, code, label) in [
        ("five_hour", "5h", "ClinePass · 5 小时"),
        ("weekly", "weekly", "ClinePass · 每周"),
        ("monthly", "monthly", "ClinePass · 每月"),
    ] {
        let rows = limits
            .iter()
            .filter(|row| row["type"] == kind)
            .collect::<Vec<_>>();
        if rows.len() > 1 {
            return Err(Error::http(
                502,
                "ClinePass 返回了重复订阅窗口，未发布冲突读数",
            ));
        }
        let Some(row) = rows.first() else { continue };
        let used = account::number(&row["percentUsed"])
            .filter(|used| *used >= 0.0)
            .ok_or_else(|| Error::http(502, "ClinePass 窗口用量缺失，未记录为零消耗"))?;
        windows.push(
            json!({ "key": format!("cline-pass:{kind}"), "group": "ClinePass", "window": code,
            "label": label, "unit": "%", "remainingPercent": (100.0 - used).clamp(0.0, 100.0),
            "usedPercent": used, "remaining": null, "limit": null, "unlimited": false,
            "resetsAt": row.get("resetsAt").and_then(account::expires), "models": models }),
        );
    }
    Ok(windows)
}

fn check_user(data: &Value, expected: &str) -> Result<()> {
    let returned = account::text(data, &["userId"]);
    if !returned.is_empty() && returned != expected {
        return Err(changed());
    }
    Ok(())
}

fn credits(token: &str, user_id: &str) -> Result<f64> {
    let data = client::request(
        "GET",
        &format!("/users/{}/balance", client::encode(user_id)),
        Some(token),
        true,
        None,
    )?
    .data()?;
    check_user(&data, user_id)?;
    account::number(&data["balance"]).ok_or_else(|| {
        Error::http(
            502,
            "Cline 没有返回有效的余额读数，未记录为余额为零或查询成功",
        )
        .with_code("cline_usage_invalid_response")
    })
}

pub fn query() -> Result<Value> {
    let before = sdk::account()?;
    let secret = refresh::ensure(false)?;
    let record = sdk::account()?;
    if ["id", "provider", "identity", "desktop"]
        .iter()
        .any(|key| before[*key] != record[*key])
    {
        return Err(changed());
    }
    let token = account::access(&secret)?;
    let current = refresh::snapshot()?;
    if account::access(&current)? != token
        || account::identity(&current) != account::identity(&secret)
    {
        return Err(changed());
    }
    check(&record)?;
    let stored = account::text(&secret, &["account"]);
    let mut user_id = if stored.starts_with("usr-")
        || account::api_key(&secret) && !stored.starts_with("token-")
    {
        stored
    } else {
        account::user_id(&token)
    };
    if user_id.is_empty() {
        let data = client::request("GET", "/users/me", Some(&token), true, None)?.data()?;
        user_id = account::text(&data, &["id"]);
    }
    if user_id.is_empty() || user_id.len() > 512 || user_id.chars().any(char::is_control) {
        return Err(Error::http(502, "Cline 余额查询未取得账号标识"));
    }
    let declared = if account::api_key(&secret) {
        String::new()
    } else {
        account::user_id(&token)
    };
    if !declared.is_empty() && declared != user_id {
        return Err(changed());
    }
    check(&record)?;
    let mut warnings = Vec::new();
    let raw = match credits(&token, &user_id) {
        Ok(raw) => Some(raw),
        Err(error) if error.code.as_deref() == Some("identity_changed") => return Err(error),
        Err(error) if crate::PROVIDER_ID == "cline-pass" => {
            warnings.push(format!("Cline 按量 credit 未读取：{}", error.message));
            None
        }
        Err(error) => return Err(error),
    };
    check(&record)?;
    let credits = raw.map(|raw| raw / 1_000_000.0);
    let plan = match client::request("GET", "/users/me/plan", Some(&token), true, None) {
        Ok(reply) if reply.status == 404 => None,
        Ok(reply) => match reply.data() {
            Ok(plan) if plan.is_object() => Some(plan),
            Ok(_) => {
                warnings.push("Cline 套餐响应结构无效，套餐状态保留未知".into());
                None
            }
            Err(error) => {
                warnings.push(format!("Cline 套餐信息未读取：{}", error.message));
                None
            }
        },
        Err(error) => {
            warnings.push(format!("Cline 套餐信息未读取：{}", error.message));
            None
        }
    };
    check(&record)?;
    let mut subscription = json!({});
    if let Some(plan) = plan {
        let mut name = account::text(&plan, &["displayName", "name"]);
        if name.is_empty() {
            name = account::text(&plan["plan"], &["displayName", "name"]);
        }
        if !name.is_empty() {
            subscription["planName"] = json!(name.chars().take(256).collect::<String>());
        }
        for key in ["currentPeriodEnd", "cancelAt"] {
            if let Some(expiry) = plan
                .get(key)
                .filter(|value| !value.is_null())
                .and_then(account::expires)
            {
                subscription["expireAt"] = json!(expiry);
                break;
            }
        }
        let status = account::text(&plan, &["status"]);
        if !status.is_empty() {
            subscription["status"] = json!(status.chars().take(80).collect::<String>());
        }
    }
    let wallets = credits.into_iter().map(|credits| json!({ "type": "cline_credit", "unit": "credit",
        "displayName": if credits < 0.0 { "Cline credit（已欠费）" } else { "Cline credit" }, "balance": credits })).collect::<Vec<_>>();
    let mut windows = Vec::new();
    if crate::PROVIDER_ID == "cline-pass" {
        match client::request(
            "GET",
            "/users/me/plan/usage-limits",
            Some(&token),
            true,
            None,
        ) {
            Ok(reply) if reply.status == 404 => {
                subscription["quotaStatus"] = json!("inactive");
            }
            Ok(reply) => match reply.data().and_then(|data| subscription_windows(&data)) {
                Ok(readings) => {
                    subscription["quotaStatus"] = json!(if readings.is_empty() {
                        "unreported"
                    } else {
                        "reported"
                    });
                    windows = readings;
                }
                Err(error) => {
                    warnings.push(format!("ClinePass 订阅窗口未读取：{}", error.message));
                    subscription["quotaStatus"] = json!("unknown");
                }
            },
            Err(error) => {
                warnings.push(format!("ClinePass 订阅窗口未读取：{}", error.message));
                subscription["quotaStatus"] = json!("unknown");
            }
        }
        check(&record)?;
    }
    if credits.is_none() && windows.is_empty() {
        return Err(Error::http(
            502,
            "ClinePass 未取得有效订阅窗口或按量余额，未记录为零或查询成功",
        )
        .with_code("cline_usage_invalid_response"));
    }
    let mut report = json!({ "unit": "credit", "available": credits, "wallets": wallets, "subscription": subscription,
        "readOnly": true, "partial": !warnings.is_empty(), "warnings": warnings,
        "raw": { "userId": user_id, "balance": raw, "balanceMicrocredits": raw } });
    if crate::PROVIDER_ID == "cline-pass" {
        report["windows"] = json!(windows);
    }
    check(&record)?;
    refresh::confirm(&secret)?;
    report["quotaSources"] = sources(&report);
    Ok(report)
}
