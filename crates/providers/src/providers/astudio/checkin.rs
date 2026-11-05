//! AStudio 每日签到：执行桌面端启动时的账号初始化，触发每日登录奖励。
//!
//! ── 发奖动作与到账核对 ──────────────────────────────────────
//! 本地 `auth.rs` 的登录收尾已使用 POST `tenant-app/v2/init-app`。
//! 参考 `2951461586/AStudio2API` README §4.1 的客户端协议与实测记录：这个
//! 无 body、带 Cookie 的 POST 才触发每日奖励；查询会员/积分或关闭弹窗不发奖。
//! 读余额 → 初始化账号 → 等 2.5 秒 → 再读余额，分别核对通用与星火积分。
//! 余额没涨不等于今日已领，也不能断言没有活动；只报告本次未观测到新到账。

use agent2api_provider_sdk::PluginCtx;
use serde_json::Value;

use agent2api_provider_protocol as proto;

use super::accounts::Credentials;
use super::client::{self, StudioError};
use super::{endpoints, PROVIDER_ID};

const REWARD_WAIT_MS: u64 = 2500;

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
    // 不能用只读查询替代每日登录动作；上游拒绝时直接返回，不能继续报「余额没涨」。
    if let Err(error) = client::init_app(&credentials).await {
        return match error {
            StudioError::Expired(message) => Err(message),
            StudioError::Failed(message) => Ok(proto::CheckinReport {
                outcome: proto::CheckinOutcome::Failed,
                credit: None,
                streak: None,
                message: Some(message),
            }),
        };
    }
    tokio::time::sleep(std::time::Duration::from_millis(REWARD_WAIT_MS)).await;
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
    let deltas = [
        ("通用积分", balance_delta(before.points, after.points)),
        ("星火积分", balance_delta(before.spark, after.spark)),
    ];
    if deltas.iter().all(|(_, delta)| delta.is_none()) {
        return Ok(proto::CheckinReport {
            outcome: proto::CheckinOutcome::Failed,
            credit: None,
            streak: None,
            message: Some("签到前后积分读数不完整，无法确认奖励到账".into()),
        });
    }
    // 两个余额池独立核对：只到账星火积分也算成功，缺失读数不能当成 0。
    let rewards: Vec<String> = deltas
        .iter()
        .filter_map(|(label, delta)| {
            delta
                .filter(|value| *value > 0.0)
                .map(|value| format!("{label} +{value}"))
        })
        .collect();
    if !rewards.is_empty() {
        let credit = rewards.join("，");
        return Ok(proto::CheckinReport {
            outcome: proto::CheckinOutcome::Claimed,
            credit: Some(credit.clone()),
            streak: None,
            message: Some(format!("每日登录奖励已到账：{credit}")),
        });
    }
    // 初始化成功不是领奖凭证；未到账保留重试，不能按「今日无活动」结束。
    Ok(proto::CheckinReport {
        outcome: proto::CheckinOutcome::Failed,
        credit: None,
        streak: None,
        message: Some("每日登录已执行，但未观测到新增积分（可能今日已领取或暂未到账）".into()),
    })
}

struct Balance {
    points: Option<f64>,
    spark: Option<f64>,
}

async fn read_balance(credentials: &Credentials) -> Result<Balance, StudioError> {
    let value = client::studio_json(credentials, endpoints::POINTS_BALANCE, "积分余额").await?;
    let balance = Balance {
        points: balance_number(value.get("totalBalance")),
        spark: balance_number(value.get("sparkTotalBalance")),
    };
    if balance.points.is_none() && balance.spark.is_none() {
        return Err(StudioError::Failed("积分余额读数不完整".into()));
    }
    Ok(balance)
}

fn balance_number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|text| text.trim().parse::<f64>().ok()))
        })
        .filter(|number| number.is_finite() && *number >= 0.0)
}

fn balance_delta(before: Option<f64>, after: Option<f64>) -> Option<f64> {
    let delta = after? - before?;
    delta.is_finite().then_some(delta)
}
