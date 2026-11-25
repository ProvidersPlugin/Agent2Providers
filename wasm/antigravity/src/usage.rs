//! 真实额度窗口、逐模型配额与订阅档；结合 fetchAvailableModels 与 retrieveUserQuotaSummary。
//! 上游没有绝对余量时不编造 available=0，不造签到或领取。

use super::{
    account, argon, catalog_shape, control, credentials, errors, model_quotas, models, project,
    refresh, usage_windows, util,
};
use agent2api_wasm_sdk::{json, Error, Result, Value};

fn is_terminal_auth_error(error: &Error) -> bool {
    error.status == Some(401) || error.code.as_deref() == Some("antigravity_account_action")
}

fn should_abort_usage(error: &Error) -> bool {
    is_terminal_auth_error(error) || errors::local(error) || errors::transport_failed(error)
}

fn load_project_meta(body: &Value) -> (String, String, Option<Value>) {
    let project_id = body["cloudaicompanionProject"]
        .as_str()
        .or_else(|| body["cloudaicompanionProject"]["id"].as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let tier = project::tier(body);
    let credits = body
        .pointer("/paidTier/availableCredits/0/creditAmount")
        .and_then(|v| v.as_f64().or_else(|| v.as_str()?.trim().parse().ok()))
        .filter(|amount| amount.is_finite() && *amount >= 0.0)
        .map(|amount| {
            let text = if (amount.fract()).abs() < 1e-6 {
                format!("{}", amount.round() as i64)
            } else {
                format!("{amount:.2}")
            };
            json!({
                "type": "ai_credits",
                "displayName": "AI Credits",
                "balance": amount,
                "balanceView": text,
            })
        });
    (project_id, tier, credits)
}

pub fn query() -> Result<Value> {
    let record = refresh::ensure(false, None)?;
    let secret = credentials::parse(&record["secret"])?;
    let token = credentials::token(&secret)?;
    let mut prior_transport_failures = 0;
    let load_body = match project::load(&token) {
        Ok(body) => Some(body),
        Err(error) if is_terminal_auth_error(&error) || errors::local(&error) => return Err(error),
        Err(error) => {
            if errors::transport_failed(&error) {
                prior_transport_failures = 1;
            }
            None
        }
    };
    let (discovered_project, raw_tier, ai_credits) = load_body
        .as_ref()
        .map(load_project_meta)
        .unwrap_or_default();
    let stored_project = util::text(&secret, &["projectId"]);
    let effective_project = if stored_project.is_empty() {
        discovered_project
    } else {
        stored_project
    };

    let models_res = control::fetch_for_project_with_transport_failures(
        &secret,
        &effective_project,
        false,
        prior_transport_failures,
        || account::unchanged(&record["secret"]),
    );
    if let Err(ref error) = models_res {
        if should_abort_usage(error) {
            return Err(error.clone());
        }
    }

    let summary_res = control::fetch_for_project(&secret, &effective_project, true, || {
        account::unchanged(&record["secret"])
    });
    if let Err(ref error) = summary_res {
        if is_terminal_auth_error(error) || errors::local(error) {
            return Err(error.clone());
        }
    }

    let (models_body, summary_body, warnings) = match (models_res, summary_res) {
        (Ok(m), Ok(s)) => (Some(m), Some(s), Vec::new()),
        (Ok(m), Err(_)) => (
            Some(m),
            None,
            vec!["分组汇总暂未返回，已按模型列表显示可用额度".to_string()],
        ),
        (Err(_), Ok(s)) => (
            None,
            Some(s),
            vec!["单模型配额明细暂未返回，以下为分组汇总读数".to_string()],
        ),
        (Err(primary), Err(_)) => return Err(primary),
    };

    account::unchanged(&record["secret"])?;

    let mut catalog = models_body
        .as_ref()
        .map(catalog_shape::parse)
        .filter(|items| !items.is_empty())
        .unwrap_or_else(|| models::for_account(&record).unwrap_or_else(|_| catalog_shape::fallback()));
    argon::supplement(&mut catalog, summary_body.as_ref());

    let group_readings = model_quotas::collect_groups(summary_body.as_ref());
    let per_model = model_quotas::build(models_body.as_ref(), &group_readings);

    let paid_tier = load_body.is_some()
        && !raw_tier.trim().is_empty()
        && usage_windows::normalize_tier(&raw_tier) != "Free";
    let (mut wallets, windows, available) = if let Some(ref summary) = summary_body {
        let parsed = usage_windows::wallets(summary, &catalog, paid_tier);
        if parsed.1.is_empty() && !per_model.is_empty() {
            usage_windows::fallback_from_models(&per_model)
        } else {
            parsed
        }
    } else {
        usage_windows::fallback_from_models(&per_model)
    };
    if let Some(credit_wallet) = ai_credits {
        wallets.push(credit_wallet);
    }

    let mut result = json!({
        "availableView": available,
        "unit": "%",
        "wallets": wallets,
        "windows": windows,
        "modelQuotas": per_model,
        "raw": {},
    });
    if let Some(ref summary) = summary_body {
        result["raw"]["quotaSummary"] = summary.clone();
    }
    if !warnings.is_empty() {
        result["warnings"] = json!(warnings);
    }
    if load_body.is_some() {
        let plan = if raw_tier.is_empty() {
            "Free".into()
        } else {
            usage_windows::normalize_tier(&raw_tier)
        };
        result["raw"]["tier"] = json!(raw_tier);
        result["subscription"] = json!({ "planName": plan });
    }
    result["quotaSources"] = usage_windows::sources(&result);
    Ok(result)
}
