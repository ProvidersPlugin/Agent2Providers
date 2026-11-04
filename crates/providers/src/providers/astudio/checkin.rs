//! AStudio 每日签到：上游没有「领取」接口，签到 = 当天会话首次活跃的发放。
//!
//! ── 为什么用「前后余额差」观测 ──────────────────────────────
//! 桌面端的弹窗只做通知，服务端在账号会话当天首次活跃时直接加积分。没有
//! 可调用的领取接口，就只能：读余额 → 发几个与桌面端启动同类的只读请求
//! （触发活跃）→ 等 2.5 秒 → 再读余额。**涨了才算签到成功**；没涨如实报
//! 「未观测到新发放」，绝不谎称已签（那会让签到记录变成没发生过的断言）。

use agent2api_provider_sdk::PluginCtx;
use serde_json::Value;

use agent2api_provider_protocol as proto;

use super::accounts::Credentials;
use super::client::{self, StudioError};
use super::{endpoints, PROVIDER_ID};

const TOUCH_WAIT_MS: u64 = 2500;

pub async fn checkin(ctx: &PluginCtx, account: &str) -> Result<proto::CheckinReport, String> {
    let stored = ctx
        .store_get(PROVIDER_ID, account)
        .await
        .map_err(|error| format!("读取 AStudio 账号失败：{error}"))?;
    let credentials = Credentials::from_secret(&stored.secret);
    if credentials.key.is_empty() {
        return Err("AStudio 账号缺少 modelBearerToken，请重新登录或粘贴".to_string());
    }
    if !credentials.has_session() {
        return Ok(proto::CheckinReport {
            outcome: proto::CheckinOutcome::Ineligible,
            credit: None,
            streak: None,
            message: Some("该账号只有模型凭据（无网页会话），无法执行每日登录签到".into()),
        });
    }
    let config = match client::studio_json(&credentials, endpoints::PLAN_CONFIG, "签到配置").await
    {
        Ok(value) => value,
        Err(StudioError::Expired(message)) => return Err(message),
        Err(error) => {
            return Ok(proto::CheckinReport {
                outcome: proto::CheckinOutcome::Failed,
                credit: None,
                streak: None,
                message: Some(error.message().to_string()),
            })
        }
    };
    if config.get("dailyLoginEnabled").and_then(Value::as_bool) == Some(false) {
        return Ok(proto::CheckinReport {
            outcome: proto::CheckinOutcome::Inactive,
            credit: None,
            streak: None,
            message: Some("当前没有每日登录积分活动".into()),
        });
    }
    let before = match read_balance(&credentials).await {
        Ok(value) => value,
        Err(StudioError::Expired(message)) => return Err(message),
        Err(error) => {
            return Ok(proto::CheckinReport {
                outcome: proto::CheckinOutcome::Failed,
                credit: None,
                streak: None,
                message: Some(error.message().to_string()),
            })
        }
    };
    // 会话活跃 touch：与桌面端启动时同类只读请求，失败不影响观测。
    let _ = client::studio_json(&credentials, endpoints::MEMBERSHIP_ME, "会员信息").await;
    let _ = client::studio_json(&credentials, endpoints::POINTS_SUMMARY, "积分汇总").await;
    tokio::time::sleep(std::time::Duration::from_millis(TOUCH_WAIT_MS)).await;
    let after = match read_balance(&credentials).await {
        Ok(value) => value,
        Err(StudioError::Expired(message)) => return Err(message),
        Err(error) => {
            return Ok(proto::CheckinReport {
                outcome: proto::CheckinOutcome::Failed,
                credit: None,
                streak: None,
                message: Some(error.message().to_string()),
            })
        }
    };
    let before_total = before.get("totalBalance").and_then(Value::as_f64);
    let after_total = after.get("totalBalance").and_then(Value::as_f64);
    let (Some(before_total), Some(after_total)) = (before_total, after_total) else {
        return Ok(proto::CheckinReport {
            outcome: proto::CheckinOutcome::Failed,
            credit: None,
            streak: None,
            message: Some("积分余额读数不完整".into()),
        });
    };
    let delta = after_total - before_total;
    if delta > 0.0 {
        return Ok(proto::CheckinReport {
            outcome: proto::CheckinOutcome::Claimed,
            credit: Some(format!("{delta}")),
            streak: None,
            message: Some(format!("每日登录积分 +{delta}")),
        });
    }
    Ok(proto::CheckinReport {
        outcome: proto::CheckinOutcome::Inactive,
        credit: None,
        streak: None,
        message: Some("未观测到新发放：余额无变化（发放可能挂靠桌面端登录）".into()),
    })
}

async fn read_balance(credentials: &Credentials) -> Result<Value, StudioError> {
    client::studio_json(credentials, endpoints::POINTS_BALANCE, "积分余额").await
}
