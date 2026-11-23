//! Qoder 国内额度：套餐 + 个人资源包 + 可用组织资源包，缺失值不补零。
//! 单位和套餐到期保持上游口径，不读千问办公钱包，不访问国际站。
use super::{account, client, endpoints, refresh, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

fn addon(raw: &Value) -> Option<&Value> {
    let bucket = raw.get("addOnQuota").filter(|value| value.is_object())?;
    ["total", "used", "remaining"]
        .iter()
        .any(|key| util::number(&bucket[*key]).is_some_and(|value| value > 0.0))
        .then_some(bucket)
}
fn org(raw: &Value) -> Option<&Value> {
    let bucket = raw
        .get("orgResourcePackage")
        .filter(|value| value.is_object())?;
    let available = bucket["available"].as_bool().unwrap_or_else(|| {
        ["cap", "total", "used", "remaining"]
            .iter()
            .any(|key| util::number(&bucket[*key]).is_some_and(|value| value > 0.0))
    });
    available.then_some(bucket)
}
fn document(raw: &Value) -> Result<Value> {
    if !["userQuota", "addOnQuota", "orgResourcePackage"]
        .iter()
        .any(|key| raw[*key].is_object())
    {
        return Err(Error::http(
            502,
            "Qoder 额度响应没有有效额度桶，未报告为零余额",
        ));
    }
    let unit = raw["userQuota"]["unit"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or("额度");
    let mut credits = Vec::new();
    let mut wallets = Vec::new();
    let mut sources = Vec::new();
    let mut remaining = Vec::new();
    for (kind, name, bucket) in [
        (
            "user",
            "套餐额度",
            raw.get("userQuota").filter(|value| value.is_object()),
        ),
        ("addon", "个人资源包", addon(raw)),
        ("org", "组织资源包", org(raw)),
    ] {
        let Some(bucket) = bucket else {
            continue;
        };
        let total = if kind == "org" {
            util::number(&bucket["cap"]).or_else(|| util::number(&bucket["total"]))
        } else {
            util::number(&bucket["total"])
        };
        let used = util::number(&bucket["used"]);
        let balance = util::number(&bucket["remaining"]);
        let bucket_unit = bucket["unit"]
            .as_str()
            .filter(|value| !value.is_empty())
            .unwrap_or(unit);
        if let Some(balance) = balance.filter(|_| bucket_unit == unit) {
            remaining.push(balance);
        }
        let mut row = json!({ "name": name });
        for (key, value) in [("total", total), ("used", used), ("remain", balance)] {
            if let Some(value) = value {
                row[key] = json!(value);
            }
        }
        if bucket_unit != unit {
            row["unit"] = json!(bucket_unit);
        }
        if [total, used, balance].iter().any(Option::is_some) {
            credits.push(row);
        }
        for (suffix, label, value) in [("total", "总额", total), ("used", "已用", used)] {
            if let Some(value) = value {
                wallets.push(json!({ "type": format!("{kind}_{suffix}"), "displayName": format!("{name}{label}"),
                    "balance": value, "balanceView": format!("{value} {bucket_unit}") }));
            }
        }
        if total.is_some() || balance.is_some() || used.is_some() {
            sources.push(
                json!({ "id": kind, "provider": endpoints::PROVIDER, "label": name,
                "remaining": balance, "total": total, "used": used, "unit": bucket_unit }),
            );
        }
    }
    let available = remaining
        .into_iter()
        .reduce(|left, right| left + right)
        .filter(|value| value.is_finite());
    Ok(
        json!({ "available": available, "unit": unit, "wallets": wallets, "credits": credits, "quotaSources": sources }),
    )
}
pub fn query() -> Result<Value> {
    let mut snapshot = refresh::ensure(false)?;
    let mut reply = client::get(endpoints::USAGE, &snapshot.credentials.access, "额度查询")?;
    if reply.status == 401 && snapshot.credentials.can_refresh() {
        account::unchanged(&snapshot)?;
        let original = snapshot.credentials.binding();
        snapshot = refresh::ensure(true)?;
        if snapshot.credentials.binding() != original {
            return Err(account::changed());
        }
        reply = client::get(endpoints::USAGE, &snapshot.credentials.access, "额度查询")?;
    }
    let raw = client::accepted(reply, "额度查询")?;
    let mut report = document(&raw)?;
    let mut subscription = json!({});
    match client::get(endpoints::PLAN, &snapshot.credentials.access, "套餐查询")
        .and_then(|reply| client::accepted(reply, "套餐查询"))
    {
        Ok(plan) => {
            let name = util::text(&plan, &["plan_tier_name"]);
            if !name.is_empty() {
                subscription["planName"] = json!(name);
            }
        }
        Err(error) if util::terminal(&error) => return Err(error),
        Err(error) => {
            report["subscriptionError"] = json!(error.message);
        }
    }
    if let Some(value) = raw.get("expiresAt").filter(|value| !value.is_null()) {
        if let Some(at) = util::timestamp(value).filter(|at| *at < 4_102_444_800_000) {
            subscription["expireAt"] = json!(at);
        }
    }
    account::unchanged(&snapshot)?;
    report["subscription"] = subscription;
    Ok(report)
}
pub fn sources(report: &Value) -> Value {
    report
        .get("quotaSources")
        .filter(|value| value.is_array())
        .cloned()
        .unwrap_or_else(|| json!([]))
}
