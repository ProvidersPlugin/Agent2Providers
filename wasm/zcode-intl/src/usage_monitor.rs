//! 开放平台窗口查询：仅认证拒绝时从 Bearer 换裸令牌，比例不能伪装成绝对余额。
use super::network::{self, compact, integer, number, text, MONITOR};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn fetch(token: &str) -> Result<Value> {
    for raw in [false, true] {
        let authorization = if raw {
            token.to_string()
        } else {
            format!("Bearer {token}")
        };
        let reply = network::request(
            "GET",
            &format!("{MONITOR}/api/monitor/usage/quota/limit"),
            json!({ "Authorization": authorization }),
            None,
        )?;
        if matches!(reply.status, 401 | 403) {
            if !raw {
                continue;
            }
            return Err(Error::http(401, "ZCode 编码套餐密钥被上游拒绝，请重新登录"));
        }
        let code = integer(&reply.value["code"]).unwrap_or(0);
        if matches!(code, 401 | 403) {
            if !raw {
                continue;
            }
            return Err(Error::http(401, "ZCode 编码套餐密钥已失效"));
        }
        if !(200..300).contains(&reply.status) || code != 0 {
            return Err(Error::http(502, "ZCode 窗口额度查询未成功"));
        }
        let data = &reply.value["data"];
        if !data.is_object() {
            return Err(Error::http(502, "ZCode 窗口额度响应格式无效"));
        }
        return Ok(normalize(data));
    }
    Err(Error::http(401, "ZCode 窗口额度查询被拒绝"))
}

fn plan(data: &Value) -> String {
    let name = text(
        data,
        &["planName", "plan", "plan_type", "planType", "packageName"],
    );
    if !name.is_empty() {
        return name;
    }
    let name = text(data, &["level"]);
    let lower = name.to_lowercase();
    if lower.contains("max") {
        "Max".into()
    } else if lower.contains("pro") {
        "Pro".into()
    } else if lower.contains("lite") {
        "Lite".into()
    } else {
        name
    }
}
fn percent(value: f64) -> String {
    format!("{value:.1}").trim_end_matches(".0").to_string()
}
fn period(row: &Value) -> (String, &str) {
    match integer(&row["unit"]) {
        Some(3) => (
            format!("每 {} 小时", integer(&row["number"]).unwrap_or(5)),
            "hours",
        ),
        Some(4) => ("每天".into(), "daily"),
        Some(5) => ("每月".into(), "monthly"),
        Some(6) => ("每周".into(), "weekly"),
        _ => ("每周期".into(), "cycle"),
    }
}

fn normalize(data: &Value) -> Value {
    let name = plan(data);
    let mut wallets = Vec::new();
    let mut tightest: Option<(f64, String)> = None;
    let mut absolute = None;
    for row in data["limits"].as_array().into_iter().flatten() {
        let kind = text(row, &["type"]);
        let (period, code) = period(row);
        let label = match kind.as_str() {
            "TOKENS_LIMIT" => "提示次数",
            "TIME_LIMIT" => "工具调用",
            "" => "额度",
            _ => &kind,
        };
        let unit = if matches!(kind.as_str(), "TOKENS_LIMIT" | "TIME_LIMIT") {
            "次"
        } else {
            ""
        };
        let total = number(&row["usage"]);
        let used = number(&row["currentValue"]);
        let remaining =
            number(&row["remaining"]).or_else(|| total.zip(used).map(|(total, used)| total - used));
        let used_percent = number(&row["percentage"])
            .or_else(|| {
                total
                    .zip(used)
                    .filter(|(total, _)| *total > 0.0)
                    .map(|(total, used)| used / total * 100.0)
            })
            .map(|value| value.clamp(0.0, 100.0));
        let remaining_percent = used_percent.map(|value| 100.0 - value);
        let mut view = match total.zip(used) {
            Some((total, used)) => format!(
                "剩 {} / {} {unit}",
                compact(remaining.unwrap_or((total - used).max(0.0))),
                compact(total)
            )
            .trim_end()
            .to_string(),
            None => remaining_percent
                .map(|value| format!("剩 {}%", percent(value)))
                .unwrap_or_else(|| "已读取".into()),
        };
        let reset = integer(&row["nextResetTime"])
            .filter(|value| *value > 0)
            .map(|value| {
                if value > 100_000_000_000 {
                    value
                } else {
                    value.saturating_mul(1000)
                }
            });
        if let Some(reset) = reset.and_then(network::clock_label) {
            view.push_str(&format!(" · {reset}（北京时间）重置"));
        }
        if let Some(value) = remaining_percent {
            if tightest.as_ref().is_none_or(|(old, _)| value < *old) {
                tightest = Some((value, period.clone()));
            }
        } else if absolute.is_none() {
            absolute = remaining.map(|remaining| {
                format!("剩 {} {unit}", compact(remaining))
                    .trim_end()
                    .to_string()
            });
        }
        wallets.push(json!({ "type": format!("zcode_window_{code}"), "displayName": format!("{label}（{period}）"),
            "balance": remaining, "balanceView": view, "total": total, "used": used,
            "remainingPercent": remaining_percent, "planName": if name.is_empty() { None } else { Some(&name) }, "resetAt": reset }));
    }
    let headline = tightest
        .map(|(value, period)| format!("{period}剩 {}%", percent(value)))
        .or(absolute)
        .unwrap_or_else(|| {
            if wallets.is_empty() {
                "无额度窗口".into()
            } else {
                format!("已读取 {} 项额度窗口", wallets.len())
            }
        });
    let subscription = if name.is_empty() {
        json!({})
    } else {
        json!({ "planName": name })
    };
    json!({ "availableView": headline, "unit": "", "wallets": wallets, "subscription": subscription,
        "source": "api.z.ai/api/monitor", "quotaSources": super::usage_sources::monitor(data) })
}
