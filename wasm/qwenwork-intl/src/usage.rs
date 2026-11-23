//! 国际账号额度按原生 account-context 的 user/plan/quota 形状读取，字段兼容性属基线推断。
//! 原生未给国际钱包取证；此包没有钱包请求或国内三池推断，缺数值不补零。
use super::{account, auth, client, endpoints, refresh, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

fn document(raw: &Value) -> Result<Value> {
    let data = client::unwrap_data(raw);
    if !["user", "plan", "quota"]
        .iter()
        .any(|key| data.get(*key).is_some())
    {
        return Err(Error::http(
            502,
            "千问办公国际余额响应不符合基线账号上下文形状，未报告为零余额",
        ));
    }
    let quota = data
        .get("quota")
        .filter(|value| value.is_object())
        .ok_or_else(|| Error::http(502, "千问办公国际没有返回有效额度资料，未报告为零余额"))?;
    let primary = quota
        .get("user_quota")
        .or_else(|| quota.get("userQuota"))
        .filter(|value| value.is_object())
        .unwrap_or(quota);
    let total = util::number(&quota["total"])
        .or_else(|| util::number(&primary["total"]))
        .filter(|value| *value >= 0.0);
    let remaining = util::number(&quota["remaining"])
        .or_else(|| util::number(&primary["remaining"]))
        .filter(|value| *value >= 0.0);
    let used = util::number(&quota["used"])
        .or_else(|| util::number(&primary["used"]))
        .filter(|value| *value >= 0.0)
        .or_else(|| match (total, remaining) {
            (Some(total), Some(remaining)) if total >= remaining => {
                Some(total - remaining).filter(|value| value.is_finite())
            }
            _ => None,
        });
    let remaining = remaining.or_else(|| match (total, used) {
        (Some(total), Some(used)) if total >= used => Some(total - used),
        _ => None,
    });
    let mut credits = Vec::new();
    let mut sources = Vec::new();
    if [total, used, remaining].iter().any(Option::is_some) {
        let mut row = json!({ "name": "积分" });
        let mut source = json!({ "id": "quota", "provider": endpoints::PROVIDER, "label": "积分", "unit": "credits" });
        for (field, source_field, number) in [
            ("total", "total", total),
            ("used", "used", used),
            ("remain", "remaining", remaining),
        ] {
            if let Some(number) = number {
                row[field] = json!(number);
                source[source_field] = json!(number);
            }
        }
        credits.push(row);
        sources.push(source);
    }
    let mut subscription = json!({});
    if let Some(plan) = data.get("plan").filter(|value| value.is_object()) {
        let name = util::text(plan, &["name"]);
        if !name.is_empty() {
            subscription["planName"] = json!(name);
        }
        if let Some(at) = util::timestamp(&plan["next_due_date"])
            .or_else(|| util::timestamp(&plan["nextDueDate"]))
        {
            subscription["expireAt"] = json!(at);
        }
        if let Some(at) = util::timestamp(&plan["next_refresh_date"])
            .or_else(|| util::timestamp(&plan["nextRefreshDate"]))
        {
            subscription["resetAt"] = json!(at);
        }
    }
    Ok(
        json!({ "available": remaining, "unit": "credits", "credits": credits, "wallets": [], "quotaSources": sources,
        "subscription": subscription, "walletsStatus": "not_verified", "walletsNote": endpoints::WALLETS_NOTE,
        "evidenceNote": endpoints::EVIDENCE_NOTE }),
    )
}
fn validate_identity(raw: &Value, snapshot: &account::Snapshot) -> Result<()> {
    let user_view = auth::user(raw);
    let is_biz=user_view["isBiz"].as_bool().or_else(||user_view["is_biz"].as_bool());
    if snapshot.credentials.is_biz.is_some()&&is_biz.is_some()&&snapshot.credentials.is_biz!=is_biz {
        return Err(account::changed());
    }
    for (expected,keys) in [(&snapshot.credentials.organization,&["organizationId","organization_id","orgId","org_id"][..]),
        (&snapshot.credentials.team,&["teamId","team_id"][..])] {
        let actual=util::text(user_view,keys);
        if !expected.is_empty()&&!actual.is_empty()&&expected!=&actual{return Err(account::changed());}
    }
    let user = util::text(user_view, &["id", "userId", "user_id", "uid"]);
    if !user.is_empty() && user != snapshot.credentials.user {
        return Err(account::changed());
    }
    account::unchanged(snapshot)
}
pub fn query() -> Result<Value> {
    let mut snapshot = refresh::ensure(false)?;
    let mut reply = client::get(
        endpoints::USAGE,
        &snapshot.credentials.access,
        "国际余额查询",
    )?;
    // 原生账户上下文在 401/403 后只同账号续期一次；不改推理的裸 403 分类。
    if matches!(reply.status, 401 | 403) && snapshot.credentials.can_refresh() {
        account::unchanged(&snapshot)?;
        let original = snapshot.credentials.binding();
        snapshot = refresh::ensure(true)?;
        if snapshot.credentials.binding() != original {
            return Err(account::changed());
        }
        reply = client::get(
            endpoints::USAGE,
            &snapshot.credentials.access,
            "国际余额查询",
        )?;
    }
    let raw = client::accepted(reply, "国际余额查询")?;
    validate_identity(&raw, &snapshot)?;
    document(&raw)
}
pub fn sources(report: &Value) -> Value {
    report
        .get("quotaSources")
        .filter(|value| value.is_array())
        .cloned()
        .unwrap_or_else(|| json!([]))
}
