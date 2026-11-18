use super::{account, network::number, usage_billing, usage_monitor};
use agent2api_wasm_sdk::{json, Error, Result, Value};

fn has_wallets(value: &Value) -> bool {
    value["wallets"]
        .as_array()
        .is_some_and(|items| !items.is_empty())
}

pub fn query() -> Result<Value> {
    let mut record = account::record()?;
    let jwt = account::jwt(&record["secret"]);
    let access = account::access(&record["secret"]);
    let mut failures = Vec::new();
    if !jwt.is_empty() {
        record = account::with_device()?;
        let current_access = account::access(&record["secret"]);
        if current_access != access || account::jwt(&record["secret"]) != jwt {
            return Err(Error::http(409, "ZCode 账号凭据已变化，请重新查询用量"));
        }
        match usage_billing::fetch(&record) {
            Ok(mut billing) => {
                if account::plan(&record["secret"]) == account::CODING
                    && !has_wallets(&billing)
                    && !access.is_empty()
                {
                    match usage_monitor::fetch(&access) {
                        Ok(monitor) if has_wallets(&monitor) => {
                            billing["wallets"] = monitor["wallets"].clone();
                            billing["availableView"] = monitor["availableView"].clone();
                            billing["walletsFrom"] = json!("monitor");
                            if let (Some(left), Some(right)) = (
                                billing["quotaSources"].as_array_mut(),
                                monitor["quotaSources"].as_array(),
                            ) {
                                left.extend(right.iter().cloned());
                            }
                        }
                        Err(error) if error.code.as_deref() == Some("forbidden") => {
                            return Err(error)
                        }
                        _ => {}
                    }
                }
                account::require_current(&record)?;
                return Ok(with_credits(billing));
            }
            Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
            Err(error) => failures.push(error),
        }
    }
    if account::plan(&record["secret"]) == account::CODING && !access.is_empty() {
        match usage_monitor::fetch(&access) {
            Ok(value) => {
                account::require_current(&record)?;
                return Ok(with_credits(value));
            }
            Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
            Err(error) => failures.push(error),
        }
    }
    if failures.is_empty() {
        return Err(
            Error::http(400, "未配置查询凭据，请补充套餐 JWT 或编码套餐 API Key")
                .with_code("usage_not_configured"),
        );
    }
    let index = failures
        .iter()
        .position(|error| error.status == Some(401))
        .unwrap_or(0);
    Err(failures.swap_remove(index))
}

fn with_credits(mut report: Value) -> Value {
    let credits = report["wallets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|wallet| {
            let total = number(&wallet["total"]);
            let used = number(&wallet["used"]);
            let remain = number(&wallet["balance"]);
            if total.is_none() && used.is_none() && remain.is_none() {
                return None;
            }
            let mut row = json!({ "name": wallet["displayName"].as_str().unwrap_or("") });
            for (key, value) in [("total", total), ("used", used), ("remain", remain)] {
                if let Some(value) = value {
                    row[key] = json!(value);
                }
            }
            if let Some(name) = wallet["planName"]
                .as_str()
                .filter(|name| !name.trim().is_empty())
            {
                row["grantReason"] = json!(name);
            }
            Some(row)
        })
        .collect::<Vec<_>>();
    if !credits.is_empty() {
        report["credits"] = json!(credits);
    }
    report
}

pub fn sources(report: &Value) -> Value {
    report["quotaSources"]
        .as_array()
        .map(|items| json!(items))
        .unwrap_or_else(|| json!([]))
}
