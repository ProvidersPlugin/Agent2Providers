//! KukuAI 每日签到：优先「库库 30 天签到」，旧 freepoint 任务作兜底。
//!
//! ── 上游是什么（2026-10-08 真实凭证实测 + kuku2api 同款）────────
//! ```text
//! GET  /api/genflowpro/freepoint/homenew?{WEB_QUERY}
//!      → {errno, data:{activities:[{activity_key, activity_name, period_no,
//!         tabs:[{tasks:[{task_type, task_name, task_status, single_reward_point,
//!         claimable_point}]}]}]}}
//! POST /api/genflowpro/freepoint/taskComplete?{WEB_QUERY}
//!      body: task_type=LOGIN|CHAT        （x-www-form-urlencoded）
//!      → {errno, data:{complete_status, reward_point}}
//! ```
//! 实测要点：
//!   - `homenew` 的任务状态 `FINISHED` / `UNFINISHED`，`claimable_point` 恒 0
//!     —— **不能拿它当领取门槛**（「完成一次对话」UNFINISHED 照样领到 +50）；
//!   - `taskComplete` **幂等**：重复领返回 `SUCCESS` + `reward_point=0`（或
//!     kuku2api 记录的 `BADGE_ALREADY_RECEIVED`），因此「已领过」不算失败；
//!   - 每日可自助完成的是 `LOGIN`（每日登录）与 `CHAT`（完成一次对话）；
//!     `INVITE_VISIT` / `INVITE_DOWNLOAD` 要真实受邀人，接口上强领只会失败，
//!     不进领取清单（kuku2api 同款取舍）。
//!
//! ── 2026-10-21 起的 30 天签到（主链路）──────────────────────
//! 旧 freepoint 活动结束后，上游主签到换成「库库 30 天签到」：
//! ```text
//! GET  /act/api/conf?conf_key=kukuSignConf
//!      → data[].conf_value.{activityTid, signPoint}
//! GET  /act/v2/kukuai30/homepage?tid=…
//!      → {signed_today, continuous_days, total_days, …}
//! GET  /operation/activitys/api/getJsToken
//!      → {jsToken: "function fn(a){…};fn(\"HEX\")"}（URL 编码）
//! POST /act/v2/kukuai30/signin?tid=…&jsToken=…&aid=15459
//!      → {continuous, signed_today:true}；已签返回 errno=10152
//! ```
//! 实测缺 `jsToken` 返回 8002、缺 `aid` 返回 8001，两者齐全即成功。
//! 签到只认网页 Cookie（不需要换发业务 STOKEN），所以放在 freepoint 之前。
//!
//! ── 会话前提 ────────────────────────────────────────────────
//! freepoint 是业务接口，与 userreport 同一道会话门：调 `with_business_stoken`
//! 先换发 genflowpro STOKEN（`engine.rs`），老账号 / 通行证级 STOKEN 也能签。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;
use crate::logging;

use super::credentials;
use super::http;
use super::{BASE_URL, USER_AGENT, WEB_QUERY};

/// 每日可自助完成的任务（类型，展示名）。展示名用于结果文案。
const DAILY_TASKS: [(&str, &str); 2] = [("LOGIN", "每日登录"), ("CHAT", "完成一次对话")];

/// 30 天签到活动配置（`conf_value.activityTid` / `signPoint`）。
const ACTIVITY_CONF_URL: &str = "/act/api/conf?conf_key=kukuSignConf";

/// 30 天签到活动页。风控接口按来源页校验，Referer 不能省。
const ACTIVITY_REFERER: &str = "https://kuku.baidu.com/operation/activitys/kukuPcSignAct/main";

/// 活动风控的 `aid`（活动配置 `dataApp.app_key`）。签到接口缺它会返回 8001。
const ACTIVITY_AID: i64 = 15459;

/// 每日签到积分的兜底值（配置里 `signPoint` 读不到时使用）。
const ACTIVITY_SIGN_POINT: u32 = 10;

/// 30 天签到链路的结果。
enum ThirtyDaySignin {
    Signed { continuous: u32, total: u32, point: u32 },
    Already { continuous: u32, total: u32 },
    /// 活动未配置 / 已结束 / 面板不可读：交给旧的 freepoint 链路。
    Unavailable,
    Failed(String),
}

/// 30 天签到活动用的请求头。Referer 必须是活动页 —— 风控按来源页判定。
fn activity_headers(credentials: &credentials::KukuCredentials) -> Vec<(String, String)> {
    vec![
        ("Cookie".to_string(), credentials.cookie_header()),
        ("User-Agent".to_string(), USER_AGENT.to_string()),
        ("Referer".to_string(), ACTIVITY_REFERER.to_string()),
        ("Origin".to_string(), BASE_URL.to_string()),
        ("Accept-Language".to_string(), "zh-CN,zh;q=0.9".to_string()),
    ]
}

/// 30 天签到：读活动配置 → 看今日状态 → 取风控 token → 调签到。
///
/// 上游把「活动配置」和「签到接口」拆成了两个域：
/// - `/act/api/conf?conf_key=kukuSignConf` 给 `activityTid` 与每日积分；
/// - `/act/v2/kukuai30/homepage?tid=…` 给今日是否已签、连续天数；
/// - `/operation/activitys/api/getJsToken` 给风控 token（URL 编码的 JS 片段）；
/// - `/act/v2/kukuai30/signin?tid=…&jsToken=…&aid=15459` 真正签到。
///
/// 实测缺 `aid` 会返回 8001（账号异常），缺 `jsToken` 会返回 8002；
/// 两者齐全即成功。活动页地址固定，`aid` 是活动 `app_key`。
async fn signin_thirty_days(credentials: &credentials::KukuCredentials) -> ThirtyDaySignin {
    let headers = activity_headers(credentials);
    let conf_url = format!("{BASE_URL}{ACTIVITY_CONF_URL}");
    let conf = match http::get_json_value(&conf_url, &headers, None, Some(30_000)).await {
        Ok(value) => value,
        Err(error) => {
            logging::log(
                "[Checkin]",
                &format!("KukuAI 30 天签到：活动配置读取失败（{}）", error.message),
            );
            return ThirtyDaySignin::Unavailable;
        }
    };
    let Some((tid, point)) = activity_config(&conf) else {
        return ThirtyDaySignin::Unavailable;
    };

    let home_url = format!("{BASE_URL}/act/v2/kukuai30/homepage?tid={tid}");
    let home = match http::get_json_value(&home_url, &headers, None, Some(30_000)).await {
        Ok(value) => value,
        Err(error) => {
            logging::log(
                "[Checkin]",
                &format!("KukuAI 30 天签到：活动面板读取失败（{}）", error.message),
            );
            return ThirtyDaySignin::Unavailable;
        }
    };
    if home.get("errno").and_then(Value::as_i64).unwrap_or(-1) != 0 {
        // 151004 / -6 是上游的「无活动」；其它错误也按不可用处理，交给旧链路。
        return ThirtyDaySignin::Unavailable;
    }
    let data = home.get("data").cloned().unwrap_or(Value::Null);
    let continuous = data
        .get("continuous_days")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let total = data
        .get("total_days")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    if data
        .get("signed_today")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return ThirtyDaySignin::Already { continuous, total };
    }

    let token_url = format!("{BASE_URL}/operation/activitys/api/getJsToken");
    let token_value = match http::get_json_value(&token_url, &headers, None, Some(30_000)).await {
        Ok(value) => value,
        Err(error) => {
            return ThirtyDaySignin::Failed(format!("签到凭证获取失败：{}", error.message))
        }
    };
    let Some(token) = token_value
        .get("data")
        .and_then(|data| data.get("jsToken"))
        .and_then(Value::as_str)
        .and_then(extract_js_token)
    else {
        return ThirtyDaySignin::Failed("签到凭证为空（jsToken 解析失败）".to_string());
    };

    let sign_url =
        format!("{BASE_URL}/act/v2/kukuai30/signin?tid={tid}&jsToken={token}&aid={ACTIVITY_AID}");
    let signed = match http::post_form_value(&sign_url, &headers, &[], None, Some(30_000)).await {
        Ok(value) => value,
        Err(error) => return ThirtyDaySignin::Failed(format!("签到请求失败：{}", error.message)),
    };
    let errno = signed.get("errno").and_then(Value::as_i64).unwrap_or(-1);
    if errno == 0 {
        let data = signed.get("data").cloned().unwrap_or(Value::Null);
        let continuous = data
            .get("continuous")
            .and_then(Value::as_u64)
            .unwrap_or(continuous as u64) as u32;
        let total = total.max(continuous);
        if data
            .get("signed_today")
            .and_then(Value::as_bool)
            .unwrap_or(true)
        {
            return ThirtyDaySignin::Signed {
                continuous,
                total,
                point,
            };
        }
        return ThirtyDaySignin::Failed("上游未确认签到完成".to_string());
    }
    if errno == 10152 {
        return ThirtyDaySignin::Already { continuous, total };
    }
    let message = signed
        .get("show_msg")
        .or_else(|| signed.get("errmsg"))
        .and_then(Value::as_str)
        .unwrap_or("未知错误");
    if errno == 8001 || errno == 8002 {
        return ThirtyDaySignin::Failed(format!(
            "上游风控未通过（errno={errno}：{message}），请重新登录该账号后再试"
        ));
    }
    ThirtyDaySignin::Failed(format!("errno={errno}：{message}"))
}

/// 从活动配置里取 `activityTid` 与每日积分。
fn activity_config(value: &Value) -> Option<(String, u32)> {
    let rows = value.get("data")?.as_array()?;
    for row in rows {
        let Some(config) = row.get("conf_value") else {
            continue;
        };
        let Some(tid) = config.get("activityTid").and_then(|value| {
            value
                .as_str()
                .map(str::to_string)
                .or_else(|| value.as_i64().map(|number| number.to_string()))
        }) else {
            continue;
        };
        if tid.is_empty() {
            continue;
        }
        let point = config
            .get("signPoint")
            .and_then(|value| {
                value
                    .as_str()
                    .and_then(|text| text.parse::<u32>().ok())
                    .or_else(|| value.as_u64().map(|number| number as u32))
            })
            .unwrap_or(ACTIVITY_SIGN_POINT);
        return Some((tid, point));
    }
    None
}

/// 风控 token 是 URL 编码的 JS 片段：`function fn(a){...};fn("HEX")`。
/// 取最后一对引号里的值即可，不执行任何 JS。
fn extract_js_token(raw: &str) -> Option<String> {
    let decoded = percent_decode(raw);
    let end = decoded.rfind('"')?;
    let start = decoded[..end].rfind('"')?;
    let token = &decoded[start + 1..end];
    (!token.is_empty()).then(|| token.to_string())
}

/// 只解码 `%XX`（风控 token 片段只用到这一层编码）。
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                out.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// KukuAI 账号的每日签到（自动签到框架的 claim）。
///
/// 返回 `{success, alreadyCompleted, outcome, credit, streak, msg}` —— 与
/// WorkBuddy / 小浣熊 / AutoClaw / Qoder / Loomy 的 claim 同一形状
/// （`billing::checkin` 的汇总只认 success 与 msg，其它字段供界面展示）。
pub async fn claim_daily_checkin(
    ctx: &PluginCtx,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let credentials = credentials::snapshot_for(ctx, account_id).await?;

    // 0) 30 天签到（2026-10-21 起的主签到活动）。旧的「免费领积分」活动
    //    在部分账号上已经没有任务面板（`taskComplete` 恒 FAIL），先走新链路；
    //    只有新活动不可用时才回落到旧任务。
    match signin_thirty_days(&credentials).await {
        ThirtyDaySignin::Signed {
            continuous,
            total,
            point,
        } => {
            let msg = format!(
                "库库 30 天签到成功：+{point} 积分（连续 {continuous} 天，累计 {total} 天）"
            );
            logging::log("[Checkin]", &msg);
            return Ok(json!({
                "success": true,
                "alreadyCompleted": false,
                "outcome": "claimed",
                "credit": format!("+{point} 积分"),
                "streak": continuous,
                "msg": msg,
            }));
        }
        ThirtyDaySignin::Already { continuous, total } => {
            let msg = format!("今日已签到（连续 {continuous} 天，累计 {total} 天）");
            logging::log("[Checkin]", &format!("KukuAI {msg}"));
            return Ok(json!({
                "success": false,
                "alreadyCompleted": true,
                "outcome": "done",
                "streak": continuous,
                "msg": msg,
            }));
        }
        ThirtyDaySignin::Failed(message) => {
            logging::log(
                "[Checkin]",
                &format!("⚠️ KukuAI 30 天签到失败：{message}"),
            );
            return Ok(json!({
                "success": false,
                "alreadyCompleted": false,
                "outcome": "failed",
                "msg": format!("库库 30 天签到失败：{message}"),
            }));
        }
        ThirtyDaySignin::Unavailable => {}
    }

    let prepared = credentials::with_business_stoken(&credentials).await;
    let headers = credentials::request_headers(&prepared);

    // 1) 任务面板：先探一次会话（freepoint 与 userreport 同一道门，-6 就是
    //    登录态问题），顺带确认活动面板可读。
    let panel_url = format!("{BASE_URL}/api/genflowpro/freepoint/homenew?{WEB_QUERY}");
    let panel = http::get_json_value(&panel_url, &headers, None, Some(30_000)).await?;
    let errno = panel.get("errno").and_then(Value::as_i64).unwrap_or(-1);
    if errno != 0 {
        let message = panel
            .get("show_msg")
            .or_else(|| panel.get("errmsg"))
            .and_then(Value::as_str)
            .unwrap_or("未知错误");
        return Err(GatewayError::with_status(
            401,
            format!("KukuAI 签到失败（freepoint errno={errno}：{message}）——请重新登录该账号"),
        ));
    }

    // 旧活动已经结束 / 该账号没有活动时，面板是 `{activities: []}`。
    // 这时再打 taskComplete 只会拿到一组 FAIL，误报成「签到失败」；
    // 直接按「没有可领活动」返回。
    let activity_count = panel
        .get("data")
        .and_then(|data| data.get("activities"))
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    if activity_count == 0 {
        let msg = "当前没有可参加的「免费领积分」活动（活动可能已结束）";
        logging::log("[Checkin]", &format!("KukuAI {msg}"));
        return Ok(json!({
            "success": false,
            "alreadyCompleted": false,
            "outcome": "inactive",
            "msg": msg,
        }));
    }

    // 2) 逐个领取。单个任务失败只记日志继续（一个任务被风控不该拖垮另
    //    一个），与「个别账号查询失败不再拖垮整轮定时查询」同一取舍。
    let mut newly: Vec<String> = Vec::new();
    let mut already_claimed = false;
    let mut total = 0i64;
    let mut failures: Vec<String> = Vec::new();
    let mut unresolved: Vec<String> = Vec::new();
    for (task_type, task_label) in DAILY_TASKS {
        let url = format!("{BASE_URL}/api/genflowpro/freepoint/taskComplete?{WEB_QUERY}");
        let value = match http::post_form_value(
            &url,
            &headers,
            &[("task_type", task_type)],
            None,
            Some(30_000),
        )
        .await
        {
            Ok(value) => value,
            Err(error) => {
                logging::log(
                    "[Checkin]",
                    &format!("⚠️ KukuAI「{task_label}」领取失败：{}", error.message),
                );
                failures.push(format!("{task_label}：{}", error.message));
                continue;
            }
        };
        let errno = value.get("errno").and_then(Value::as_i64).unwrap_or(-1);
        if errno != 0 {
            let message = value
                .get("show_msg")
                .or_else(|| value.get("errmsg"))
                .and_then(Value::as_str)
                .unwrap_or("未知错误");
            logging::log(
                "[Checkin]",
                &format!("⚠️ KukuAI「{task_label}」领取失败（errno={errno}：{message}）"),
            );
            failures.push(format!("{task_label}（errno={errno}：{message}）"));
            continue;
        }
        let data = value.get("data").cloned().unwrap_or(Value::Null);
        let status = data
            .get("complete_status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let reward = data.get("reward_point").and_then(Value::as_i64);
        if let Some(reward) = reward.filter(|reward| *reward > 0 && status.eq_ignore_ascii_case("SUCCESS")) {
            newly.push(format!("{task_label}+{reward}"));
            total = total.saturating_add(reward);
        } else if (status.eq_ignore_ascii_case("SUCCESS") && reward == Some(0))
            || status.eq_ignore_ascii_case("BADGE_ALREADY_RECEIVED") {
            already_claimed = true;
        } else {
            unresolved.push(format!("{task_label}：上游未确认完成（{status}）"));
        }
    }

    let success = !newly.is_empty();
    let done = !success && already_claimed;
    let mut msg = if success {
        format!("签到完成：{}（共 +{total}）", newly.join("、"))
    } else if done {
        "今日积分已领过（任务接口幂等，无新增）".to_string()
    } else if !failures.is_empty() {
        format!("每日任务领取失败：{}", failures.join("；"))
    } else {
        "本次没有已确认完成的每日任务".to_string()
    };
    if success || done {
        if !failures.is_empty() { msg.push_str(&format!("；部分任务失败：{}", failures.join("；"))); }
    }
    if !unresolved.is_empty() { msg.push_str(&format!("；{}", unresolved.join("；"))); }
    logging::log("[Checkin]", &msg);
    Ok(json!({
        "success": success,
        "alreadyCompleted": done,
        "outcome": if success { "claimed" } else if done { "done" } else if failures.is_empty() { "inactive" } else { "failed" },
        "msg": msg,
    }))
}
