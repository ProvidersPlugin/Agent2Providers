//! 新版 quota/summary 为主，只有 HTTP 404/405 回退 overview；不因错误补空额度。
//! 只读活动附在标准 usage 结果中，沿用 balanceView/warnings 展示，不开启领取按钮。

use agent2api_wasm_sdk::{json, Result, Value};

use crate::{
    account::{self, Snapshot},
    activities,
    endpoints::ReadEndpoint,
    errors,
    quota_windows::{self, Windows},
    util,
};

const SECTIONS: &[&str] = &[
    "window_limit_section",
    "enterprise_window_limit_section",
    "quota_package_section",
    "incentive_quota_section",
];

fn data(value: &Value) -> Result<Value> {
    let data = util::object_data(value, "豆包工作没有返回有效额度汇总对象")?;
    if !["member_info"]
        .iter()
        .chain(SECTIONS.iter())
        .any(|key| data.get(*key).is_some_and(|value| !value.is_null()))
    {
        return Err(errors::protocol(
            "豆包工作额度汇总结构无法识别，不能当作空结果或余额为零",
        ));
    }
    if data
        .get("member_info")
        .is_some_and(|value| !value.is_null() && !value.is_object())
    {
        return Err(errors::protocol("豆包工作会员信息结构无效"));
    }
    for field in ["hasActiveSubscription", "hasEnterpriseSubscription"] {
        if data["member_info"]
            .get(field)
            .is_some_and(|value| !value.is_null() && !value.is_boolean())
        {
            return Err(errors::protocol("豆包工作会员状态格式无效，未猜测额度池"));
        }
    }
    let has_membership = ["hasActiveSubscription", "hasEnterpriseSubscription"]
        .iter()
        .any(|field| data["member_info"][*field].is_boolean());
    let has_section = SECTIONS
        .iter()
        .any(|key| data.get(*key).is_some_and(Value::is_object));
    if !has_membership && !has_section {
        return Err(errors::protocol(
            "豆包工作额度汇总没有可识别的会员状态或额度窗口",
        ));
    }
    Ok(data)
}

struct Section<'a> {
    rows: Vec<quota_windows::Limit<'a>>,
    complete: bool,
}

fn section<'a>(data: &'a Value, key: &str, warnings: &mut Vec<String>) -> Section<'a> {
    match quota_windows::limits(&data[key]) {
        Ok(rows) => {
            let complete = data[key]["window_limit_groups"].as_array().is_some_and(|groups|
                groups.iter().all(|group| group["window_limits"].is_array()));
            if data[key].is_object() && !complete {
                warnings.push(format!("额度分区 {key} 未下发完整窗口，相关读数保留未知，不影响其他已返回窗口"));
            }
            Section { rows, complete }
        }
        Err(error) => {
            warnings.push(format!("额度分区 {key} 未能解析：{}；该分区保持未知", error.message));
            Section { rows: Vec::new(), complete: false }
        }
    }
}

pub fn read(account: &Snapshot) -> Result<Value> {
    let body = json!({ "product_line": "membership" });
    let (summary, overview) = match account.read(ReadEndpoint::QuotaSummary, &body) {
        Ok(value) => {
            data(&value)?;
            let overview = account.read(ReadEndpoint::Subscription, &body);
            (value, overview)
        }
        Err(error) if matches!(error.status, Some(404 | 405)) => {
            let overview = account.read(ReadEndpoint::Subscription, &body)?;
            (overview.clone(), Ok(overview))
        }
        Err(error) => return Err(error),
    };
    if let Err(error) = &overview {
        if errors::host_stop(error) || error.status == Some(401) {
            return Err(error.clone());
        }
    }
    let data = data(&summary)?;
    let mut warnings: Vec<String> = Vec::new();
    let personal = section(&data, "window_limit_section", &mut warnings);
    let enterprise = section(&data, "enterprise_window_limit_section", &mut warnings);
    let package = section(&data, "quota_package_section", &mut warnings);
    let incentive = section(&data, "incentive_quota_section", &mut warnings);
    let has_personal = data["member_info"]["hasActiveSubscription"].as_bool();
    let has_enterprise = data["member_info"]["hasEnterpriseSubscription"].as_bool();
    let enterprise_usable =
        quota_windows::usable(&data["enterprise_window_limit_section"], &enterprise.rows);
    let personal_usable = quota_windows::usable(&data["window_limit_section"], &personal.rows);
    let selection = match (has_personal, has_enterprise) {
        (Some(true), Some(false)) => Some("personal"),
        (Some(false), Some(true)) => Some("enterprise"),
        (Some(true), Some(true)) if enterprise_usable => Some("enterprise"),
        (Some(true), Some(true)) if !enterprise.complete || !personal.complete => None,
        (Some(true), Some(true)) => if personal_usable { Some("personal") } else { Some("enterprise") },
        (Some(false), Some(false)) => Some("none"),
        // 缺失的会员状态不等于 false，不能据此替账号选另一个额度池。
        _ => None,
    };
    let mut windows = Windows::default();
    if has_personal.is_none() || has_enterprise.is_none() {
        warnings.push(
            "会员身份信息不完整，未将缺失状态视为未订阅；额度池归属无法确认的部分保留未知".into(),
        );
    } else if selection.is_none() {
        warnings.push("个人和企业额度分区不完整，未把未知窗口当作不可用并切换额度池".into());
    }
    let chosen: &[quota_windows::Limit<'_>] = match selection {
        Some("enterprise") => &enterprise.rows,
        Some("personal") => &personal.rows,
        _ => &[],
    };
    for (index, row) in chosen.iter().enumerate() {
        if matches!(row.kind, 1 | 2 | 4) {
            windows.push(row, index, false, &account.credentials.protected)?;
        } else {
            windows.unknown_window = true;
        }
    }
    if matches!(selection, Some("enterprise" | "personal")) && chosen.is_empty() {
        warnings.push("当前订阅没有下发可识别窗口，余额仍未知".into());
    }
    if selection.is_some() && selection != Some("enterprise") {
        for (index, row) in package.rows.iter().enumerate() {
            if row.kind == 3 {
                windows.push(row, index, false, &account.credentials.protected)?;
            } else {
                windows.unknown_window = true;
            }
        }
        let mut bonus_count = 0;
        for (index, row) in incentive.rows.iter().enumerate() {
            if row.kind == 3 {
                bonus_count += 1;
                windows.push(row, index, true, &account.credentials.protected)?;
            } else {
                windows.unknown_window = true;
            }
        }
        if bonus_count > 1 || !incentive.complete {
            windows.bonus = None;
        }
    }
    if windows.missing_amount {
        warnings.push("部分额度窗口未提供有效剩余读数，显示未知而不是 0".into());
    }
    if windows.invalid_amount {
        warnings.push("部分额度数字无效，未把异常值当作 0".into());
    }
    if windows.invalid_date {
        warnings.push("部分额度时间格式或单位无法确认，未生成到期日期".into());
    }
    if windows.unknown_window {
        warnings.push("存在尚未识别的额度窗口类型，未把它换算成积分或 Token".into());
    }
    let plan = overview
        .as_ref()
        .ok()
        .and_then(|value| quota_windows::plan(value, &account.credentials.protected))
        .or_else(|| quota_windows::plan(&summary, &account.credentials.protected));
    if let Err(error) = &overview {
        warnings.push(format!("订阅附加信息未读到：{}", error.message));
    }
    if !windows.windows.iter().any(|window| window["remaining"].is_number()
        || window["remainingPercent"].is_number() || window["unlimited"] == true) {
        let no_subscription = selection == Some("none") && overview.as_ref().ok().is_some_and(|value| {
            let data = value.get("data").unwrap_or(value);
            data["member_info"]["hasActiveSubscription"] == false
                && data["member_info"]["hasEnterpriseSubscription"] == false
                && data["current_subscription"]["status"] == 0
                && ["entitlements", "incentive_quota_entitlements", "package_entitlement_groups",
                    "quota_package_entitlement_groups", "enterprise_package_entitlement_groups"].iter()
                    .all(|key| data[*key].as_array().is_some_and(Vec::is_empty))
        });
        if no_subscription && personal.rows.is_empty() && enterprise.rows.is_empty()
            && package.rows.is_empty() && incentive.rows.is_empty() {
            return Err(agent2api_wasm_sdk::Error::http(400,
                "豆包工作登录态有效，但上游确认当前没有个人或企业订阅，也未下发可查询的额度；这不是凭据失效，余额保持未知")
                .with_code("usage_unavailable"));
        }
        return Err(errors::protocol("豆包工作本次额度查询没有取得可确认的读数，未记录为余额为零或查询成功；请重新读取官方额度状态"));
    }
    Ok(
        json!({ "provider": "doubaowork", "unit": "额度", "available": null,
        "availableView": if windows.windows.is_empty() { "额度未知" } else { "分窗口额度" },
        "windows": windows.windows, "credits": windows.credits, "quotaSources": windows.sources,
        "subscription": { "planName": plan, "hasPersonalSubscription": has_personal,
            "hasEnterpriseSubscription": has_enterprise, "selectedQuotaPool": selection },
        "bonusLeft": windows.bonus, "tokenUsage": null, "tokenUsageKnown": false,
        "readOnly": true, "partial": !warnings.is_empty(), "warnings": warnings }),
    )
}

pub fn query() -> Result<Value> {
    let account = account::current()?;
    account.verify()?;
    let mut report = read(&account)?;
    match activities::read(&account) {
        Ok(activity) => {
            report["balanceView"] = json!(activity.summary);
            report["activities"] = activity.value;
        }
        Err(error) if errors::host_stop(&error) || error.status == Some(401) => return Err(error),
        Err(error) => {
            let warnings = report["warnings"]
                .as_array_mut()
                .ok_or_else(|| errors::protocol("豆包工作额度展示结果无效"))?;
            warnings.push(json!(format!("活动只读查询未完成：{}", error.message)));
            report["activities"] = json!({ "state": "unknown", "readOnly": true,
                "completed": null, "error": errors::report(&error) });
        }
    }
    account.unchanged()?;
    report["partial"] = json!(report["warnings"].as_array().is_some_and(|warnings| !warnings.is_empty()));
    report["queriedAt"] = json!(util::now()?);
    Ok(report)
}
