//! 本仓 codearts/balance.rs 的只读投影（AGPL-3.0-only，2026-11-10）。
//! metrics 是必需结构；只收 show=true。积分与 token 各算各的，未知与矛盾不补零。

use agent2api_wasm_sdk::{json, Result, Value};
use serde::Serialize;

use crate::{binding::Account, endpoints::ReadRequest, errors, gates, network, util};

const TOTAL: &str = "usageTotalPackageCredit";
const PARTS: [&str; 3] = [
    "usageBasicPackageCredit",
    "usageOnDemandPackageCredit",
    "usageBonusPackageCredit",
];

#[derive(Clone, PartialEq, Serialize)]
struct Meter {
    name: String,
    label: String,
    used_tokens: Option<i64>,
    allowance_tokens: Option<i64>,
    credit_total: Option<f64>,
    credit_used: Option<f64>,
    credit_remaining: Option<f64>,
    used_percent: Option<f64>,
    consistent: bool,
}
impl Meter {
    fn credit(&self) -> bool {
        self.credit_total.is_some() || self.credit_used.is_some() || self.credit_remaining.is_some()
    }
    fn view(&self) -> Option<String> {
        if let (Some(left), Some(total)) = (self.credit_remaining, self.credit_total) {
            return Some(format!("剩 {left} / {total} 积分"));
        }
        if let Some(left) = self.credit_remaining {
            return Some(format!("剩 {left} 积分"));
        }
        if self.credit() {
            return self.credit_used.map(|used| format!("已用 {used} 积分"));
        }
        self.token_view()
    }
    fn token_view(&self) -> Option<String> {
        if let Some(used) = self.used_tokens {
            if let Some(limit) = self.allowance_tokens.filter(|limit| *limit > 1) {
                return Some(format!("已用 {used} / {limit} token"));
            }
            return Some(format!("已用 {used} token"));
        }
        self.used_percent.map(|percent| format!("已用 {percent}%"))
    }
    fn wallet(&self) -> Value {
        json!({ "type": format!("meter:{}", self.name), "displayName": self.label,
            "balanceView": self.view(), "creditTotal": self.credit_total, "creditUsed": self.credit_used,
            "creditRemaining": self.credit_remaining, "usedPercent": self.used_percent,
            "consistent": self.consistent })
    }
}

pub fn query() -> Result<Value> {
    let account = Account::current()?;
    let value = network::get(&account, ReadRequest::Statistics, 2 * 1024 * 1024)?;
    let mut report = parse(&value, &account.credential.secrets())?;
    account.unchanged()?;
    report["readAt"] = json!(util::now()?);
    report["httpBudget"] = json!(gates::HTTP_BUDGET);
    Ok(report)
}

fn parse(value: &Value, secrets: &[String]) -> Result<Value> {
    if let Some(code) = value.get("error_code").filter(|code| !code.is_null()) {
        if code
            .as_str()
            .is_none_or(|code| !["", "0", "0000", "success"].contains(&code))
        {
            return Err(errors::protocol(
                "CodeArts 订阅统计返回业务错误或未知错误码，未当成空余额",
            ));
        }
    }
    let rows = value["metrics"]
        .as_array()
        .filter(|rows| rows.len() <= 256)
        .ok_or_else(|| {
            errors::protocol("CodeArts 订阅统计缺少有效 metrics 数组；未记录为无额度")
        })?;
    let mut meters: Vec<Meter> = Vec::new();
    for row in rows {
        if !row.is_object() {
            return Err(errors::protocol("CodeArts 统计条目不是对象"));
        }
        match row.get("show").filter(|value| !value.is_null()) {
            Some(Value::Bool(true)) => {}
            Some(Value::Bool(false)) | None => continue,
            _ => return Err(errors::protocol("CodeArts 统计展示开关不是布尔值")),
        }
        let name = errors::upstream_text(row, "name", secrets, 128)?;
        if name.is_empty() {
            return Err(errors::protocol("CodeArts 可见统计条目缺少名称"));
        }
        let total = number(row, "package_credit_amount")?;
        let used = number(row, "package_credit_used")?;
        let remaining = number(row, "package_credit_remain")?;
        let consistent = !total.is_some_and(|value| value < 0.0)
            && !used.is_some_and(|value| value < 0.0)
            && match (total, used, remaining) {
                (Some(total), Some(used), Some(left)) => close(total, used + left),
                _ => true,
            };
        let meter = Meter {
            label: label(&name)
                .map(str::to_string)
                .unwrap_or_else(|| name.clone()),
            name,
            used_tokens: integer(row, "usage_token_num")?,
            allowance_tokens: integer(row, "package_token_amount")?,
            credit_total: total,
            credit_used: used,
            credit_remaining: remaining,
            used_percent: number(row, "value")?.filter(|value| *value >= 0.0),
            consistent,
        };
        if let Some(previous) = meters.iter().find(|old| old.name == meter.name) {
            if *previous != meter {
                return Err(errors::protocol(
                    "CodeArts 同名统计行互相矛盾，未重复相加或覆盖旧余额",
                ));
            }
        } else {
            meters.push(meter);
        }
    }
    let (available, aggregation) = available(&meters);
    let wallets: Vec<_> = meters
        .iter()
        .filter(|meter| meter.credit())
        .map(Meter::wallet)
        .collect();
    let token_usage: Vec<_> = meters
        .iter()
        .filter(|meter| meter.used_tokens.is_some() || meter.allowance_tokens.is_some())
        .map(|meter| {
            json!({ "name": meter.name, "displayName": meter.label, "usedTokens": meter.used_tokens,
            "allowanceTokens": meter.allowance_tokens, "usedPercent": meter.used_percent,
            "balanceView": meter.token_view(), "includedInCredits": false })
        })
        .collect();
    let subscription = subscription(value, secrets)?;
    let mut report = json!({ "available": available, "unit": "积分", "wallets": wallets,
        "tokenUsage": token_usage, "subscription": subscription, "aggregation": aggregation,
        "readOnly": true, "welfareTokenPool": null,
        "note": "积分与 token 不混加；订阅统计可能含按需付费读数，查询不代表开通或付费授权" });
    if available.is_none() {
        report["message"] =
            json!("未取得无重叠且一致的可用积分总数；保留未知并展示上游分项，不当成 0 或重复累加");
    }
    report["quotaSources"] = sources(&report);
    Ok(report)
}

fn number(row: &Value, name: &str) -> Result<Option<f64>> {
    match row.get(name).filter(|value| !value.is_null()) {
        None => Ok(None),
        Some(value) => util::finite(Some(value))
            .map(Some)
            .ok_or_else(|| errors::protocol("CodeArts 统计数值字段类型无效，未转换成 0")),
    }
}
fn integer(row: &Value, name: &str) -> Result<Option<i64>> {
    match row.get(name).filter(|value| !value.is_null()) {
        None => Ok(None),
        Some(value) => value
            .as_i64()
            .map(|n| (n >= 0).then_some(n))
            .ok_or_else(|| errors::protocol("CodeArts token 用量字段类型无效")),
    }
}
fn close(left: f64, right: f64) -> bool {
    left.is_finite()
        && right.is_finite()
        && (left - right).abs() <= f64::EPSILON.max(left.abs().max(right.abs()) * 1e-6)
}

fn available(meters: &[Meter]) -> (Option<f64>, &'static str) {
    let credits: Vec<_> = meters.iter().filter(|meter| meter.credit()).collect();
    if credits.is_empty() {
        return (None, "unknown");
    }
    if credits.iter().any(|meter| !meter.consistent) {
        return (None, "inconsistent_meter");
    }
    let total = credits.iter().find(|meter| meter.name == TOTAL);
    let parts: Vec<_> = credits.iter().filter(|meter| meter.name != TOTAL).collect();
    if parts.is_empty() {
        return (
            total.and_then(|meter| meter.credit_remaining),
            "upstream_total_only",
        );
    }
    if parts
        .iter()
        .any(|meter| !PARTS.contains(&meter.name.as_str()) || meter.credit_remaining.is_none())
    {
        return (None, "unknown_overlap_or_missing_part");
    }
    let sum: f64 = parts
        .iter()
        .filter_map(|meter| meter.credit_remaining)
        .sum();
    if !sum.is_finite() {
        return (None, "invalid_sum");
    }
    match total {
        Some(total) => match total.credit_remaining {
            Some(left) if close(left, sum) => {
                (Some(left), "total_equals_disjoint_parts_counted_once")
            }
            Some(_) => (None, "total_conflicts_with_parts"),
            None => (None, "total_unknown"),
        },
        None if PARTS
            .iter()
            .all(|name| parts.iter().any(|meter| meter.name == *name)) =>
        {
            (Some(sum), "complete_visible_disjoint_parts_only")
        }
        None => (None, "partial_visible_parts_without_total"),
    }
}

fn label(name: &str) -> Option<&'static str> {
    Some(match name {
        "usageDataCodeCompletions" => "代码补全额度",
        "usageDataChatMessages" => "对话消息额度",
        "usageTokenChatMessages" => "对话 token 用量",
        TOTAL => "套餐积分",
        "usageBasicPackageCredit" => "基础包积分",
        "usageOnDemandPackageCredit" => "按需付费积分",
        "usageBonusPackageCredit" => "赠送积分",
        _ => return None,
    })
}

fn subscription(value: &Value, secrets: &[String]) -> Result<Value> {
    let reset = errors::upstream_text(value, "end_date", secrets, 128)?;
    let Some(package) = value.get("package").filter(|value| !value.is_null()) else {
        return Ok(Value::Null);
    };
    if !package.is_object() {
        return Err(errors::protocol("CodeArts 套餐字段不是对象"));
    }
    let english = errors::upstream_text(package, "package_name_en", secrets, 128)?;
    let name = if english.is_empty() {
        errors::upstream_text(package, "package_name_cn", secrets, 128)?
    } else {
        english
    };
    let code = errors::upstream_text(package, "spec_code", secrets, 128)?;
    let mut features = serde_json::Map::new();
    if let Some(rows) = package.get("features").filter(|value| !value.is_null()) {
        let rows = rows
            .as_array()
            .filter(|rows| rows.len() <= 128)
            .ok_or_else(|| errors::protocol("CodeArts 套餐功能列表无效"))?;
        for row in rows {
            let name = errors::upstream_text(row, "name", secrets, 128)?;
            if name.is_empty() {
                continue;
            }
            let enabled = match row.get("enable").filter(|value| !value.is_null()) {
                None => Value::Null,
                Some(value) if value.is_boolean() => value.clone(),
                _ => return Err(errors::protocol("CodeArts 套餐功能开关无效")),
            };
            if features.insert(name, enabled).is_some() {
                return Err(errors::protocol("CodeArts 套餐功能项重复"));
            }
        }
    }
    // 不公开或导航上游任意 package_url；固定来源只读包不新增购买入口。
    Ok(
        json!({ "planName": name, "planCode": code, "planUrl": null, "resetDate": reset, "features": features }),
    )
}

pub fn sources(report: &Value) -> Value {
    let mut sources = Vec::new();
    if let Some(remaining) = util::finite(report.get("available")) {
        sources.push(
            json!({ "id": "account", "provider": "codearts", "label": "可用积分",
            "unit": "积分", "remaining": remaining, "total": null, "used": null }),
        );
    }
    // 这些是可单独读取的官方分项，不再合成一次总和；冲突行不供限制器使用。
    for wallet in report["wallets"].as_array().into_iter().flatten() {
        let name = wallet["type"]
            .as_str()
            .and_then(|name| name.strip_prefix("meter:"))
            .unwrap_or("");
        if !PARTS.contains(&name) || wallet["consistent"] != true {
            continue;
        }
        let remaining = util::finite(wallet.get("creditRemaining"));
        let used = util::finite(wallet.get("creditUsed"));
        if remaining.is_none() && used.is_none() {
            continue;
        }
        sources.push(json!({ "id": format!("meter:{name}"), "provider": "codearts", "label": label(name),
            "unit": "积分", "remaining": remaining, "total": util::finite(wallet.get("creditTotal")), "used": used }));
    }
    Value::Array(sources)
}
