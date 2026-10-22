//! ModelScope 每日签到：**读一次登录信息就完成**。
//!
//! ── 上游是什么 ──────────────────────────────────────────────
//! 魔搭**没有独立的签到接口**（官方网页的「每日签到」按钮背后就是这条）：
//!
//! ```text
//! GET {网页站}/api/v1/users/login/info          Cookie: m_session_id=<ms-令牌>
//!   → {"Code":200,"Success":true,"Data":{"Name":"…","NickName":"…",
//!                                          "HavanaId":"…","Email":"…"}, …}
//! ```
//!
//! **调用这个端点本身就触发当日的魔粒签到**（激活当日魔粒额度），
//! 返回体里给的是账号身份，而不是「签到成功」这四个字。
//!
//! ── 幂等 ────────────────────────────────────────────────────
//! 一天内调多次是安全的（上游没有「今天已签过」的拒绝返回）。因此「今天已经
//! 触发过」**不算失败** —— 如实报成功并说明是重复调用。
//!
//! ── 两个地区打各自的站 ──────────────────────────────────────
//! `region` 决定站点。打错了会拿到 `401 access forbidden`，这里把它如实翻成
//! 「令牌不属于该站点」而不是笼统的失败。

use serde_json::Value;

use agent2api_provider_protocol as proto;

use super::client;
use super::region::Region;

/// 登录信息路径（相对网页站）。**它就是签到**，见模块头。
pub const LOGIN_INFO_PATH: &str = "/api/v1/users/login/info";

/// 每日签到。
///
/// 返回六态里的 `Claimed`（本次触发）/ `Failed`（真失败）。上游给的是身份
/// 而不是「已签到」标记，所以无法区分「今天第一次」与「今天第 N 次」——
/// 一律按 `Claimed` 报（重复调用是安全的，见模块头）。
pub async fn claim(
    region: Region,
    token: &str,
    gateway_proxy: &str,
) -> Result<proto::CheckinReport, String> {
    let (status, payload) = client::site_get(region, LOGIN_INFO_PATH, token, "每日签到", gateway_proxy).await?;
    if status == 401 || status == 403 {
        // 令牌失效 / 打错站点：如实报（网关按 TokenExpired 处置）
        return Err(client::expired_message(region, status, true));
    }
    if !(200..300).contains(&status) {
        return Ok(proto::CheckinReport {
            outcome: proto::CheckinOutcome::Failed,
            credit: None,
            streak: None,
            message: Some(format!("每日签到返回 HTTP {status}")),
        });
    }
    if !client::succeeded(&payload) {
        let message = client::message_of(&payload);
        let message = if message.trim().is_empty() {
            format!("每日签到失败（上游 Code={}）", client::code_of(&payload))
        } else {
            format!("每日签到失败：{message}")
        };
        return Ok(proto::CheckinReport {
            outcome: proto::CheckinOutcome::Failed,
            credit: None,
            streak: None,
            message: Some(message),
        });
    }

    // 成功时上游给的是账号身份（不是「签到成功」）—— 把显示名播报出来，
    // 用户一眼能确认「这条记录签的是哪个账号」（多账号场景下这是有用的信息）
    let data = client::data_of(&payload);
    let display = data
        .get("NickName")
        .or_else(|| data.get("Name"))
        .or_else(|| data.get("nickname"))
        .or_else(|| data.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("")
        .to_string();
    let message = if display.is_empty() {
        format!("魔搭{}每日签到已触发", region.label())
    } else {
        format!("魔搭{}每日签到已触发（{display}）", region.label())
    };
    Ok(proto::CheckinReport {
        outcome: proto::CheckinOutcome::Claimed,
        credit: None,
        streak: None,
        message: Some(message),
    })
}
