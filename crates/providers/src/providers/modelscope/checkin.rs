//! login/info 触发每日魔粒发放，但返回的是身份；完成状态必须由当日发放流水确认。
//! 可用余额还会受消费、冻结和过期影响，不能拿余额差当作领取金额。

use agent2api_provider_protocol as proto;

use super::balance;
use super::client::ControlClient;
use super::transactions::{self, Transactions};

pub const LOGIN_INFO_PATH: &str = "/api/v1/users/login/info";

pub async fn claim(api: &ControlClient) -> Result<proto::CheckinReport, String> {
    let day = crate::beijing::today_key();
    let before = transactions::read(api, &day).await?;
    if before.signed_on(&day) {
        return Ok(confirmed(proto::CheckinOutcome::Done, &before, None, &day));
    }
    if !before.complete {
        return Ok(proto::CheckinReport {
            outcome: proto::CheckinOutcome::Failed,
            message: Some("发放流水尚未读全，暂不触发签到，请稍后重试".to_string()),
            credit: None,
            streak: None,
        });
    }
    api.site_json(LOGIN_INFO_PATH, "每日签到").await?;
    let mut observed_day = day.clone();
    // 发放可能异步入账；短时确认未完成时保留失败态，让宿主后续重试。
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        observed_day = crate::beijing::today_key();
        let after = transactions::read(api, &observed_day).await?;
        if after.signed_on(&observed_day) {
            let gained = if observed_day == day { after.daily_increase(&before, &day) } else { None };
            let (outcome, credit) = match gained {
                Some(amount) => (proto::CheckinOutcome::Claimed, Some(amount)),
                None => (proto::CheckinOutcome::Done, None),
            };
            let mut report = confirmed(outcome, &after, credit, &observed_day);
            if let Ok(Some(balance)) = balance::current_balance(api).await {
                if let Some(message) = &mut report.message {
                    message.push_str(&format!("（当前 {} 魔粒）", transactions::amount_text(balance)));
                }
            }
            return Ok(report);
        }
    }
    Ok(proto::CheckinReport {
        outcome: proto::CheckinOutcome::Failed,
        credit: None,
        streak: None,
        message: Some(format!("登录已触达，但 {observed_day} 的签到发放尚未确认，稍后重试")),
    })
}

fn confirmed(
    outcome: proto::CheckinOutcome,
    ledger: &Transactions,
    gained: Option<f64>,
    day: &str,
) -> proto::CheckinReport {
    let message = match gained {
        Some(amount) => format!("已确认领取 {} 魔粒", transactions::amount_text(amount)),
        None => match ledger.complete.then(|| ledger.total_on(day, true)).flatten() {
            Some(amount) => format!("今日签到已确认，日常发放共 {} 魔粒", transactions::amount_text(amount)),
            None => "今日签到已确认，发放金额未读全".to_string(),
        },
    };
    proto::CheckinReport {
        outcome,
        credit: gained.map(|amount| format!("+{} 魔粒", transactions::amount_text(amount))),
        streak: None,
        message: Some(message),
    }
}
