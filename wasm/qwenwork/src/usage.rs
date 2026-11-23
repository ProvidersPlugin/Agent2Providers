//! 千问办公余额来自 account-context，个人钱包是余额组成，不再叠加到 available。
//! total/used 未知保持空；total_balance 是池余额而非池总额，真实零值与缺失分开。
use super::{account, auth, client, endpoints, refresh, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

const POOLS: [(&str, &str); 3] = [
    ("daily_credits", "每日积分"),
    ("monthly_credits", "每月积分"),
    ("longterm_credits", "长期积分"),
];
fn row(name: &str, total: Option<f64>, used: Option<f64>, remaining: Option<f64>) -> Option<Value> {
    if [total, used, remaining].iter().all(Option::is_none) {
        return None;
    }
    let mut value = json!({ "name": name });
    for (key, number) in [("total", total), ("used", used), ("remain", remaining)] {
        if let Some(number) = number {
            value[key] = json!(number);
        }
    }
    Some(value)
}
fn source(
    id: &str,
    label: &str,
    total: Option<f64>,
    used: Option<f64>,
    remaining: Option<f64>,
) -> Value {
    let mut value =
        json!({ "id": id, "provider": endpoints::PROVIDER, "label": label, "unit": "credits" });
    for (key, number) in [("total", total), ("used", used), ("remaining", remaining)] {
        if let Some(number) = number {
            value[key] = json!(number);
        }
    }
    value
}
fn document(raw: &Value, wallets: Option<&Value>) -> Result<Value> {
    let data = client::unwrap_data(raw);
    if !["user", "plan", "quota"]
        .iter()
        .any(|key| data.get(*key).is_some())
    {
        return Err(Error::http(
            502,
            "千问办公余额响应缺少有效账号上下文，未报告为零余额",
        ));
    }
    let quota = data
        .get("quota")
        .filter(|value| value.is_object())
        .unwrap_or(&Value::Null);
    let total = util::number(&quota["total"]);
    let remaining = util::number(&quota["remaining"]);
    let used = util::number(&quota["used"]).or_else(|| match (total, remaining) {
        (Some(total), Some(remaining)) => Some(total - remaining).filter(|value| value.is_finite()),
        _ => None,
    });
    let mut credits = Vec::new();
    if let Some(row) = row("积分", total, used, remaining) {
        credits.push(row);
    }
    let mut pool_sources = Vec::new();
    if let Some(wallets) = wallets {
        let data = client::unwrap_data(wallets);
        for (key, label) in POOLS {
            let bucket = &data[key];
            let balance = util::number(&bucket["total_balance"])
                .or_else(|| util::number(&bucket["totalBalance"]));
            if let Some(balance) = balance {
                if let Some(row) = row(label, None, None, Some(balance)) {
                    credits.push(row);
                }
                pool_sources.push(source(key, label, None, None, Some(balance)));
            }
        }
    }
    // 总额与分项不并列成可求和来源，部分钱包缺失也不能通过减法猜其余额。
    let quota_sources = if pool_sources.is_empty() {
        if [total, used, remaining].iter().any(Option::is_some) {
            vec![source("quota", "积分", total, used, remaining)]
        } else {
            Vec::new()
        }
    } else {
        pool_sources
    };
    let mut subscription = json!({});
    if let Some(plan) = data.get("plan").filter(|value| value.is_object()) {
        let name = util::text(plan, &["name"]);
        if !name.is_empty() {
            subscription["planName"] = json!(name);
        }
        if let Some(at) = util::timestamp(&plan["next_refresh_date"])
            .or_else(|| util::timestamp(&plan["next_due_date"]))
        {
            subscription["expireAt"] = json!(at);
        }
    }
    Ok(
        json!({ "available": remaining, "unit": "credits", "wallets": [], "credits": credits,
        "quotaSources": quota_sources, "subscription": subscription }),
    )
}
fn validate_identity(raw: &Value, snapshot: &account::Snapshot) -> Result<()> {
    let view = auth::user(raw);
    let user = util::text(view, &["id", "userId", "user_id", "uid"]);
    let is_biz = view
        .get("is_biz")
        .or_else(|| view.get("isBiz"))
        .and_then(Value::as_bool);
    let team = util::text(view, &["team_id", "teamId"]);
    let team = if team.is_empty() {
        util::text(&view["organization"], &["team_id", "teamId"])
    } else {
        team
    };
    if !user.is_empty() && user != snapshot.credentials.user
        || snapshot.credentials.is_biz.is_some()
            && is_biz.is_some()
            && snapshot.credentials.is_biz != is_biz
        || !snapshot.credentials.team.is_empty()
            && !team.is_empty()
            && snapshot.credentials.team != team
    {
        return Err(account::changed());
    }
    account::unchanged(snapshot)
}
pub fn query() -> Result<Value> {
    let mut snapshot = refresh::ensure(false)?;
    let mut renewed = false;
    let mut context = client::get(endpoints::USAGE, &snapshot.credentials.access, "余额查询")?;
    // 基线在账户上下文的 401/403 同账号续期一次，不把裸推理 403 套用这里的重试。
    if matches!(context.status, 401 | 403) && snapshot.credentials.can_refresh() {
        snapshot = renew(&snapshot)?;
        renewed = true;
        context = client::get(endpoints::USAGE, &snapshot.credentials.access, "余额查询")?;
    }
    let mut raw = client::accepted(context, "余额查询")?;
    validate_identity(&raw, &snapshot)?;
    let user = auth::user(&raw);
    let is_biz = user
        .get("is_biz")
        .or_else(|| user.get("isBiz"))
        .and_then(Value::as_bool)
        .or(snapshot.credentials.is_biz);
    let personal = is_biz != Some(true);
    let mut wallet_error = None;
    let mut wallets = None;
    if personal {
        let result = (|| -> Result<Value> {
            let mut reply = client::get(
                endpoints::WALLETS,
                &snapshot.credentials.access,
                "积分分池查询",
            )?;
            if matches!(reply.status, 401 | 403) && !renewed && snapshot.credentials.can_refresh() {
                snapshot = renew(&snapshot)?;
                raw = client::accepted(
                    client::get(endpoints::USAGE, &snapshot.credentials.access, "余额查询")?,
                    "余额查询",
                )?;
                validate_identity(&raw, &snapshot)?;
                reply = client::get(
                    endpoints::WALLETS,
                    &snapshot.credentials.access,
                    "积分分池查询",
                )?;
            }
            client::accepted(reply, "积分分池查询")
        })();
        match result {
            Ok(value) => wallets = Some(value),
            Err(error) if util::terminal(&error) => return Err(error),
            Err(error) => wallet_error = Some(error.message),
        }
    }
    let mut report = document(&raw, wallets.as_ref())?;
    if let Some(error) = wallet_error {
        report["walletsError"] = json!(error);
    }
    account::unchanged(&snapshot)?;
    Ok(report)
}
fn renew(previous: &account::Snapshot) -> Result<account::Snapshot> {
    account::unchanged(previous)?;
    let next = refresh::ensure(true)?;
    if previous.credentials.binding() != next.credentials.binding() {
        return Err(account::changed());
    }
    Ok(next)
}
pub fn sources(report: &Value) -> Value {
    report
        .get("quotaSources")
        .filter(|value| value.is_array())
        .cloned()
        .unwrap_or_else(|| json!([]))
}
