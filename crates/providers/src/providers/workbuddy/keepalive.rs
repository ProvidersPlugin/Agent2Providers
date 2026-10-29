//! WorkBuddy **国际版每日活跃**（日活领取 + 两发保活 + 网页会话）。
//!
//! ── 这是什么 ──────────────────────────────────────────────
//! 国际版有一个「每日活跃奖励」：活动开放时领一份奖励，并且要求账号当天
//! 「活跃」。活跃判定分三件事（issue #277 的抓包结论）：
//!
//! ```text
//!   ①  POST /v2/report        一条 chat_request_send 事件 → 点亮成长中心连登
//!   ②  chat/completions       一次最小对话（免费模型）→ 维持「账号有调用」
//!   ③  POST /console/as/conversations/  网页通道建带 prompt 的会话
//!                             → **每日活跃计数只认它**
//! ```
//!
//! 参考客户端把三步挂在既有签到调度器上；本仓对应关系见网关的
//! `billing::activity::workbuddy_daily_activity`（本文件是它的执行体）。
//!
//! ── 为什么两发保活长得不一样 ────────────────────────────
//! 桌面端那发必须长得和真对话一模一样：头集合与 URL 直接复用
//! [`chat_headers`] / [`chat_completions_url`] —— 两套实现必然漂移，
//! 漂移的后果是被客户端画像识别拦下。网页那发则要**剥掉全部桌面指纹**：
//! 网页链路只认 `Authorization: Bearer` 与 `X-User-Id`，UA 也是浏览器的，
//! 同一份账号凭据直接可用，无需网页登录。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use crate::errors::GatewayError;
use crate::http::{self, send_raw};
use crate::logging;
use crate::workbuddy_shared::endpoints::RESPONSE_CODE_OK;

use super::growth::WEB_UA;
use super::{chat_completions_url, chat_headers, session_of_record, web_conversations_url, Region};

/// 国际版每日活跃任务默认使用的免费模型，按可用性优先级尝试。
///
/// 这些模型在参考客户端中实测为 x0.00，不会消耗账号积分；它是**缺省值**
/// （配置里没写 `checkinKeepalive.models` 时用它）。用户自定义后的链路
/// 存在网关 config.json，读写见网关 `billing::keepalive`。
pub const DEFAULT_FREE_MODELS: &[&str] =
    &["deepseek-v4.1-flash", "hy3", "hy4-preview", "hy4-preview-f"];

/// 执行粒度（对应签到中心的三颗手动按钮）。
///
/// 定时与批量签到恒走 [`Mode::Full`]，粒度细分只是手动入口的事。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// 探测 + 条件领取 + 保活
    Full,
    /// 探测 + 条件领取
    Claim,
    /// 只保活（活动没开时只维持账号活跃）
    Keepalive,
}

impl Mode {
    /// API 层传来的 mode 字符串 → 粒度。未知值回落 [`Mode::Full`] ——
    /// 手动入口的缺省行为是「完整跑一遍」，比「悄悄少做一步」更接近点击者本意。
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("claim") => Self::Claim,
            Some("keepalive") => Self::Keepalive,
            _ => Self::Full,
        }
    }

    /// 结果行里回带的 mode（前端排查「这轮实际跑了哪几步」用）
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Claim => "claim",
            Self::Keepalive => "keepalive",
        }
    }
}

/// 保活模型链：调用方传入（网关从 config.json 读），空则回落缺省链。
///
/// ── 为什么链由网关给 ────────────────────────────────────────
/// `checkinKeepalive.models` 是**用户的**配置，落在网关的 config.json 里。
/// 插件没有那层配置（只有按家的 `settings_get`，而那一份面向 provider 行为，
/// 不是签到中心的可编辑项）。让网关把链作为 payload 传进来，配置的单一事实源
/// 就还在用户看得见的地方。
fn chain(models: &[String]) -> Vec<String> {
    let picked: Vec<String> = models
        .iter()
        .map(|model| model.trim().to_string())
        .filter(|model| !model.is_empty())
        .collect();
    if picked.is_empty() {
        DEFAULT_FREE_MODELS
            .iter()
            .map(|model| (*model).to_string())
            .collect()
    } else {
        picked
    }
}

/// 跑一次国际版每日活跃任务。
///
/// `record` 是账号的扁平 secret；`models` 是保活模型链（空 = 缺省链）。
/// 返回网关 `workbuddy_daily_activity` 的**同形 JSON**（`status` / `claim` /
/// `activity` 三段）。
pub async fn run(
    record: &Value,
    mode: Mode,
    models: &[String],
) -> Result<Value, GatewayError> {
    let session = session_of_record(record, Region::Intl);
    let models = chain(models);

    // 只保活：跳过探测与领取。读数给 **null** 而不是 false ——
    // 「没查」与「查了说没开」是两回事；前端只认 `pokeSucceeded`。
    if mode == Mode::Keepalive {
        let report = report_activity(&session).await;
        let (poke_succeeded, poke_model) = poke_daily_activity(&session, &models).await;
        let web = create_web_conversation(&session, &models).await;
        let web_ok = web.get("succeeded").and_then(Value::as_bool) == Some(true);
        if web.get("status").and_then(Value::as_i64) == Some(401)
            || (report.get("status").and_then(Value::as_i64) == Some(401) && !web_ok && !poke_succeeded) {
            return Err(GatewayError::with_status(401, "WorkBuddy 国际版保活登录态已失效"));
        }
        if !poke_succeeded && !web_ok {
            logging::verbose("[Checkin]", "WorkBuddy 国际版免费模型与网页会话保活均未成功");
        }
        // msg 交代两发保活各自的走向：网页会话才是「当日活跃」的判定通道，
        // 桌面端那发失败不致命，网页会话失败要给出原因。
        let msg = if web_ok {
            "活跃保活完成（网页会话已建）".to_string()
        } else if poke_succeeded {
            format!(
                "免费模型保活完成，但网页会话失败：{}",
                web.get("msg").and_then(Value::as_str).unwrap_or("未知原因")
            )
        } else {
            "活跃保活失败".to_string()
        };
        return Ok(json!({
            "status": Value::Null,
            "claim": {
                "success": false,
                "code": 0,
                "msg": msg,
            },
            "activity": {
                "mode": mode.as_str(),
                "active": Value::Null,
                "todayCheckedIn": Value::Null,
                "statusAvailable": Value::Null,
                "report": report,
                "pokeSucceeded": poke_succeeded,
                "pokeModel": poke_model,
                "webConversation": web,
            },
        }));
    }

    // ── ① 活动探测 ──
    let status = billing_call(&session, "POST", "/billing/meter/checkin-activity-status", None, false)
        .await;
    let (status_available, active, today_checked_in, status_value) = match status {
        Ok(call) => {
            let status_available = call.code == Some(RESPONSE_CODE_OK);
            let active = call
                .data
                .get("active")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let today_checked_in = call
                .data
                .get("today_checked_in")
                .or_else(|| call.data.get("todayCheckedIn"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !status_available {
                logging::verbose(
                    "[Checkin]",
                    &format!(
                        "WorkBuddy 国际版活跃探测未开启（code={}）",
                        call.code.unwrap_or(-1)
                    ),
                );
            }
            (
                status_available,
                active,
                today_checked_in,
                normalize_activity_status(&call),
            )
        }
        Err(error) if error.status_code == 401 => return Err(error),
        Err(error) => {
            logging::verbose(
                "[Checkin]",
                &format!("WorkBuddy 国际版活跃探测失败: {}", error.message),
            );
            (
                false,
                false,
                false,
                json!({
                    "active": false,
                    "todayCheckedIn": false,
                    "statusAvailable": false,
                    "code": -1,
                    "msg": error.message,
                }),
            )
        }
    };

    // ── ② 活跃上报（Claim / Full 都做）──
    let report = report_activity(&session).await;

    // ── ③ 条件领取 ──
    let mut claim = if today_checked_in {
        // 上游幂等，但「今日已领」就不用再打一发 —— 少一发是一发。
        json!({
            "success": false,
            "code": 0,
            "msg": "今日已领取",
            "alreadyCompleted": true,
        })
    } else if active {
        match billing_call(&session, "POST", "/v2/billing/meter/daily-checkin", None, false).await {
            Ok(call) => normalize_daily_claim(&call),
            Err(error) if error.status_code == 401 => return Err(error),
            Err(error) => {
                logging::verbose(
                    "[Checkin]",
                    &format!("WorkBuddy 国际版活跃奖励领取失败: {}", error.message),
                );
                json!({ "success": false, "code": -1, "msg": error.message })
            }
        }
    } else {
        json!({ "success": false, "code": 0, "msg": "活动未开启" })
    };
    let claim_succeeded = claim
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // ── ④ 保活（只完整模式做）──
    let (poke_succeeded, poke_model, web) = if mode == Mode::Full {
        let (succeeded, model) = poke_daily_activity(&session, &models).await;
        if !succeeded {
            logging::verbose("[Checkin]", "WorkBuddy 国际版免费模型活跃保活未成功");
        }
        let web = create_web_conversation(&session, &models).await;
        if web.get("succeeded").and_then(Value::as_bool) != Some(true) {
            logging::verbose("[Checkin]", "WorkBuddy 国际版网页会话保活未成功");
        }
        (
            Value::Bool(succeeded),
            model.map(Value::String).unwrap_or(Value::Null),
            web,
        )
    } else {
        (Value::Null, Value::Null, Value::Null)
    };
    let web_ok = web.get("succeeded").and_then(Value::as_bool) == Some(true);
    if web.get("status").and_then(Value::as_i64) == Some(401)
        || (report.get("status").and_then(Value::as_i64) == Some(401)
            && !claim_succeeded && !today_checked_in && !web_ok) {
        return Err(GatewayError::with_status(401, "WorkBuddy 国际版活跃登录态已失效"));
    }

    // 领取没有明确的成功/已领读数时，msg 给出这一轮**实际发生**的事。
    // 能走到这里的分支必然 !active：只写「已完成活跃上报」会让人以为积分
    // 该加没加 —— 两个事实都要交代。
    if mode == Mode::Full && !claim_succeeded && !today_checked_in && !active {
        let report_ok = report.get("succeeded").and_then(Value::as_bool) == Some(true);
        let poke = poke_succeeded.as_bool().unwrap_or(false);
        claim["msg"] = Value::String(if report_ok && (poke || web_ok) {
            "已完成活跃上报（连登已点亮）与保活，但活动未开启（本次没有可领的奖励）".to_string()
        } else if report_ok {
            "活跃上报完成，但保活未成功且活动未开启".to_string()
        } else if poke || web_ok {
            "活跃保活完成，但活跃上报失败且活动未开启".to_string()
        } else {
            "活跃上报与保活均未成功，且活动未开启".to_string()
        });
    }

    Ok(json!({
        "status": status_value,
        "claim": claim,
        "activity": {
            "mode": mode.as_str(),
            "active": active,
            "todayCheckedIn": today_checked_in,
            "statusAvailable": status_available,
            "report": report,
            "pokeSucceeded": poke_succeeded,
            "pokeModel": poke_model,
            "webConversation": web,
        },
    }))
}

/// 一位账号的记录 → 计费调用（`{code, msg, data}`）。
struct BillingResult {
    code: Option<i64>,
    msg: Option<String>,
    data: Value,
}

/// 调一次国际版计费接口（`path` 是 `/v2/...` 形态，落点是会话端点）。
async fn billing_call(
    session: &Value,
    method: &str,
    path: &str,
    body: Option<&Value>,
    expect_code_ok: bool,
) -> Result<BillingResult, GatewayError> {
    let endpoint = super::session_endpoint_of(session, Region::Intl);
    let url = format!("{endpoint}{path}");
    let payload_body = if method == "GET" || method == "HEAD" {
        None
    } else {
        // Node 是 `JSON.stringify(body ?? spec.body ?? {})` —— 缺省体一律是
        // 空对象（上游对缺 body 的 POST 会报 400）。
        Some(body.cloned().unwrap_or_else(|| json!({})))
    };
    let headers = super::billing::headers::build_headers(session, &[]);
    let proxy: Option<agent2api_provider_protocol::WireProxy> = session
        .get("proxy")
        .and_then(|value| serde_json::from_value(value.clone()).ok());
    let response = send_raw(
        method,
        &url,
        payload_body.as_ref(),
        &headers,
        proxy.as_ref(),
        Some(20_000),
    )
    .await
    .map_err(|error| {
        if error.is_timeout() {
            GatewayError::with_status(504, "计费接口请求超时")
        } else {
            GatewayError::with_status(502, format!("计费接口请求失败: {error}"))
        }
    })?;

    let payload = response.payload;
    let code = payload
        .as_ref()
        .and_then(|value| value.get("code"))
        .and_then(Value::as_i64);
    let msg = payload.as_ref().and_then(|value| {
        value
            .get("msg")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .or_else(|| {
                value
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
            })
            .map(str::to_string)
    });
    if response.status == 401 || response.status == 403 {
        return Err(GatewayError::with_status(
            401,
            "登录态已过期或被拒绝，无法调用计费接口",
        ));
    }
    if !response.ok {
        let detail = payload
            .as_ref()
            .and_then(|value| {
                value
                    .get("msg")
                    .and_then(Value::as_str)
                    .or_else(|| value.get("message").and_then(Value::as_str))
            })
            .unwrap_or("");
        let message = if detail.is_empty() {
            format!("计费接口返回 HTTP {}", response.status)
        } else {
            format!("计费接口返回 HTTP {}: {detail}", response.status)
        };
        return Err(GatewayError::with_status(
            i32::from(response.status),
            message,
        ));
    }
    if let Some(code_value) = code {
        if code_value != RESPONSE_CODE_OK && expect_code_ok {
            let message = msg
                .clone()
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| format!("计费接口返回 code={code_value}"));
            return Err(GatewayError::with_status(
                i32::from(response.status),
                message,
            ));
        }
    }
    let data = payload
        .as_ref()
        .and_then(|value| value.get("data"))
        .cloned()
        .unwrap_or(Value::Null);
    Ok(BillingResult { code, msg, data })
}

/// 活跃探测响应归一化为网关能直接消费的形态。
///
/// 上游字段是蛇形（`today_checked_in` / `daily_credit`），归一成驼峰与国内版
/// 签到状态的键风格一致；`statusAvailable` 区分「接口说不活动」与「接口本身
/// 没答上」—— 前者是正常状态，后者要留给排查。原始 data 原样带出（`raw`）。
fn normalize_activity_status(call: &BillingResult) -> Value {
    let data = &call.data;
    json!({
        "active": data.get("active").and_then(Value::as_bool).unwrap_or(false),
        "todayCheckedIn": data
            .get("today_checked_in")
            .or_else(|| data.get("todayCheckedIn"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "dailyCredit": data
            .get("daily_credit")
            .or_else(|| data.get("dailyCredit"))
            .cloned()
            .unwrap_or(Value::Null),
        "statusAvailable": call.code == Some(RESPONSE_CODE_OK) && !data.is_null(),
        "code": call.code,
        "msg": call.msg,
        "raw": data,
    })
}

/// 领取结果归一化为统一 claim 形状（成功带 `data` / `raw`，失败带 `code` / `msg`）。
fn normalize_daily_claim(call: &BillingResult) -> Value {
    if call.code == Some(RESPONSE_CODE_OK) && !call.data.is_null() {
        return json!({
            "success": true,
            "code": 0,
            "data": call.data,
            "raw": call.data,
        });
    }
    json!({
        "success": false,
        "code": call.code.unwrap_or(-1),
        "msg": call.msg.clone().unwrap_or_else(|| "签到失败".to_string()),
    })
}

/// 国际版对话活跃上报：`POST /v2/report` 的一条 `chat_request_send` 事件。
///
/// 事件形状照抄官方客户端的完整 24 字段，**`userId` 必填**（= 账号 uid）——
/// 缺失时上游返回 200 但**静默丢弃**事件。conversationId 无需真实会话，
/// 随机生成即可；每号每天一条（按天幂等）。
async fn report_activity(session: &Value) -> Value {
    let uid = session
        .get("account")
        .and_then(|account| account.get("uid"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if uid.is_empty() {
        return json!({
            "succeeded": false,
            "streakDays": Value::Null,
            "msg": "会话缺少账号 uid，无法上报活跃",
        });
    }
    let conversation_id = format!("agent2api-{}", crate::ids::new_message_id());
    let now = http::now_ms();
    let event = json!({
        "eventCode": "chat_request_send",
        "timestamp": now,
        "reportDelay": 0,
        "mode": "craft",
        "conversationId": conversation_id,
        "requestId": conversation_id,
        "inputLength": 12,
        "requestModelId": "deepseek-v4.1-flash",
        "requestModelName": "DeepSeek V4.1 Flash",
        "isPlan": false,
        "isAutoExecuteTerminal": false,
        "isAutoModify": false,
        "codebaseEnable": false,
        "maxToken": 0,
        "maxSteps": 0,
        "temperature": 0,
        "maxRetries": 0,
        "mentionContexts": [],
        "knowledgeId": [],
        "knowledgeName": [],
        "codebaseId": "",
        "mentionContextCount": 0,
        "command": "",
        "expertId": "",
        "recommendId": "",
        "skillId": "",
        "skillCount": 0,
        "totalCount": 0,
        "fileUri": "",
        "presentAt": now,
        "traceId": "",
        "rootRequestId": conversation_id,
        "parentConversationId": conversation_id,
        "agentName": "default",
        "agentType": "conversation",
        "userId": uid,
    });
    let body = json!([event]);
    let result = billing_call(session, "POST", "/v2/report", Some(&body), true).await;
    let succeeded = match &result {
        Ok(_) => true,
        Err(error) => {
            logging::verbose(
                "[Checkin]",
                &format!("WorkBuddy 国际版活跃上报失败: {}", error.message),
            );
            false
        }
    };
    // 回读连登天数（只读 oracle）：上报 200 ≠ 计分，闭环验证用。
    let streak_days = if succeeded {
        match read_growth_streak(session).await {
            Ok(days) => Some(days),
            Err(error) => {
                logging::verbose(
                    "[Checkin]",
                    &format!(
                        "WorkBuddy 国际版连登回读失败（上报本身已成功）: {}",
                        error.message
                    ),
                );
                None
            }
        }
    } else {
        None
    };
    let streak_text = streak_days
        .map(|days| format!("，连登 {days} 天"))
        .unwrap_or_default();
    json!({
        "succeeded": succeeded,
        "status": result.as_ref().err().map(|error| error.status_code),
        "streakDays": streak_days.map(Value::from).unwrap_or(Value::Null),
        "msg": if succeeded {
            format!("活跃上报成功{streak_text}")
        } else {
            "活跃上报失败".to_string()
        },
    })
}

/// 回读成长中心连登状态（`GET /activity/growth/streak`）→ `streak.days`。
async fn read_growth_streak(session: &Value) -> Result<i64, GatewayError> {
    let call = billing_call(session, "GET", "/activity/growth/streak", None, true).await?;
    Ok(call
        .data
        .get("streak")
        .and_then(|streak| streak.get("days"))
        .and_then(Value::as_i64)
        .unwrap_or(0))
}

/// 用免费模型发一次最小流式对话，维持账号「活跃」。
///
/// 模型链逐个尝试，第一个拿到 2xx 且响应体可读的收口；每个模型的请求与读体
/// 各自限时 —— 保活挂在批量签到的串行链上，一个挂住的模型不能把整轮拖住。
/// 响应体读到上限就算数（流式 SSE 没必要读完，但要**读**：不读的请求上游
/// 可能没记活跃）。
async fn poke_daily_activity(session: &Value, models: &[String]) -> (bool, Option<String>) {
    const TIMEOUT_MS: u64 = 20_000;
    const MAX_POKE_BYTES: usize = 1 << 20;
    for model in models {
        let body = json!({
            "model": model,
            "stream": true,
            "max_tokens": 16,
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "user", "content": "hi"},
            ],
        });
        // 头集合与真对话共用一份（chat_headers 已带全会话头族 / 链路头族与
        // Origin / Referer —— 正是渠道校验的对齐项），这里不再补丁。
        let headers = chat_headers(session, Region::Intl, Some("text/event-stream"));
        let proxy: Option<agent2api_provider_protocol::WireProxy> = session
            .get("proxy")
            .and_then(|value| serde_json::from_value(value.clone()).ok());
        let client = http::control_client(proxy.as_ref(), "direct");
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(TIMEOUT_MS),
            http::post_stream(
                &client,
                &chat_completions_url(session, Region::Intl),
                &headers,
                &body,
                "text/event-stream",
            ),
        )
        .await;
        let response = match result {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                logging::verbose(
                    "[Checkin]",
                    &format!("WorkBuddy 国际版活跃模型 {model} 请求失败: {error}"),
                );
                continue;
            }
            Err(_) => {
                logging::verbose(
                    "[Checkin]",
                    &format!("WorkBuddy 国际版活跃模型 {model} 请求超时"),
                );
                continue;
            }
        };
        if !response.is_success() {
            logging::verbose(
                "[Checkin]",
                &format!(
                    "WorkBuddy 国际版活跃模型 {model} 返回 HTTP {}",
                    response.status
                ),
            );
            continue;
        }
        let Some(raw) = response.response else {
            continue;
        };
        let body_ok = tokio::time::timeout(
            std::time::Duration::from_millis(TIMEOUT_MS),
            drain_response(raw, MAX_POKE_BYTES),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .is_some();
        if !body_ok {
            logging::verbose(
                "[Checkin]",
                &format!("WorkBuddy 国际版活跃模型 {model} 响应读取失败"),
            );
            continue;
        }
        logging::verbose(
            "[Checkin]",
            &format!("WorkBuddy 国际版活跃保活成功（model={model}）"),
        );
        return (true, Some(model.clone()));
    }
    (false, None)
}

/// 网页通道建一个带 prompt 的 agent 会话（issue #277 的修复主体）。
///
/// 国际版上游把「当日活跃」记在网页通道的会话上：`POST /console/as/conversations/`
/// 建会话即计活跃，桌面端 `chat/completions` 与 `/v2/report` 上报都**不**进
/// 这个计数。网页链路只认 Bearer + X-User-Id 两个头，同一份账号凭据直接可用。
///
/// 模型沿用同一条免费链；成功判定沿用参考实现口径 —— 响应 `code ∈ {0, 缺失}`
/// 且 `data.id` 存在。返回 `{succeeded, conversationId, msg}`，原样进
/// `activity.webConversation` 给前端与台账读。
async fn create_web_conversation(session: &Value, models: &[String]) -> Value {
    const TIMEOUT_MS: u64 = 20_000;
    const MAX_WEB_BYTES: usize = 1 << 16;
    let token = session
        .get("auth")
        .and_then(|auth| auth.get("accessToken"))
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty());
    let uid = session
        .get("account")
        .and_then(|account| account.get("uid"))
        .and_then(Value::as_str)
        .filter(|uid| !uid.is_empty());
    let (Some(token), Some(uid)) = (token, uid) else {
        // 构造失败是凭据层面的缺失（缺 token / uid），换模型也救不了
        let reason = "会话缺少账号凭据（accessToken / uid），无法走网页通道";
        logging::verbose(
            "[Checkin]",
            &format!("WorkBuddy 国际版网页会话请求构造失败: {reason}"),
        );
        return json!({ "succeeded": false, "conversationId": Value::Null, "msg": reason });
    };
    // 网页链路**只认**这两个凭据头：不带桌面端 X-IDE-* 指纹与 CLI UA，
    // UA 用浏览器串（与 growth 的 web 域同一份）。
    let headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        (
            "Accept".to_string(),
            "application/json, text/plain, */*".to_string(),
        ),
        ("User-Agent".to_string(), WEB_UA.to_string()),
        ("Origin".to_string(), "https://www.workbuddy.ai".to_string()),
        (
            "Referer".to_string(),
            "https://www.workbuddy.ai/app".to_string(),
        ),
        ("Authorization".to_string(), format!("Bearer {token}")),
        ("X-User-Id".to_string(), uid.to_string()),
    ];
    let proxy: Option<agent2api_provider_protocol::WireProxy> = session
        .get("proxy")
        .and_then(|value| serde_json::from_value(value.clone()).ok());
    let client = http::control_client(proxy.as_ref(), "direct");
    let url = web_conversations_url(session, Region::Intl);
    for model in models {
        let body = json!({
            "prompt": "Hi",
            "model": model,
            // 网页端建会话时固定带上这两项（抓包所得），保持请求形态一致
            "conversationOrigin": "workbuddy-app",
            "plugins": [{"name": "weixinpay", "marketplace": "codebuddy-builtin"}],
        });
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(TIMEOUT_MS),
            http::post_stream(&client, &url, &headers, &body, "application/json"),
        )
        .await;
        let response = match result {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                logging::verbose(
                    "[Checkin]",
                    &format!("WorkBuddy 国际版网页会话（{model}）请求失败: {error}"),
                );
                continue;
            }
            Err(_) => {
                logging::verbose(
                    "[Checkin]",
                    &format!("WorkBuddy 国际版网页会话（{model}）请求超时"),
                );
                continue;
            }
        };
        if response.status == 401 || response.status == 403 {
            return json!({ "succeeded": false, "status": 401, "msg": "WorkBuddy 国际版网页登录态已失效" });
        }
        if !response.is_success() {
            logging::verbose(
                "[Checkin]",
                &format!(
                    "WorkBuddy 国际版网页会话（{model}）返回 HTTP {}",
                    response.status
                ),
            );
            continue;
        }
        let Some(raw) = response.response else {
            continue;
        };
        let bytes = match tokio::time::timeout(
            std::time::Duration::from_millis(TIMEOUT_MS),
            consume_bytes(raw, MAX_WEB_BYTES),
        )
        .await
        {
            Ok(Ok(bytes)) => bytes,
            _ => {
                logging::verbose(
                    "[Checkin]",
                    &format!("WorkBuddy 国际版网页会话（{model}）响应读取失败"),
                );
                continue;
            }
        };
        let payload: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(error) => {
                logging::verbose(
                    "[Checkin]",
                    &format!("WorkBuddy 国际版网页会话（{model}）响应不是 JSON: {error}"),
                );
                continue;
            }
        };
        // 参考实现的判定口径：code 缺失 / null / 0 都算业务成功
        let code_ok = match payload.get("code") {
            None | Some(Value::Null) => true,
            Some(code) => code.as_i64() == Some(0),
        };
        let conversation_id = payload
            .get("data")
            .and_then(|data| data.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if !code_ok || conversation_id.is_empty() {
            let msg = payload.get("msg").and_then(Value::as_str).unwrap_or("未知原因");
            logging::verbose(
                "[Checkin]",
                &format!("WorkBuddy 国际版网页会话（{model}）未建成: msg={msg}"),
            );
            continue;
        }
        logging::verbose(
            "[Checkin]",
            &format!("WorkBuddy 国际版网页会话建成（model={model}）"),
        );
        return json!({
            "succeeded": true,
            "conversationId": conversation_id,
            "model": model,
            "msg": "网页会话已创建",
        });
    }
    json!({
        "succeeded": false,
        "conversationId": Value::Null,
        "msg": "网页会话创建失败（模型链全部未建成）",
    })
}

/// 有界读体：最多读 `max_bytes`（超出即截断返回），网络层断流算失败。
///
/// 响应体必须**读**（不读的请求上游可能没记活跃），但绝不无界缓冲。
async fn consume_bytes(response: reqwest::Response, max_bytes: usize) -> Result<Vec<u8>, ()> {
    use futures::StreamExt;

    let mut stream = response.bytes_stream();
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ())?;
        let take = chunk.len().min(max_bytes.saturating_sub(body.len()));
        body.extend_from_slice(&chunk[..take]);
        if body.len() >= max_bytes {
            return Ok(body);
        }
    }
    Ok(body)
}

/// [`consume_bytes`] 的「只求读过」形态（流式 SSE 用：字节内容不关心）。
async fn drain_response(response: reqwest::Response, max_bytes: usize) -> Result<(), ()> {
    consume_bytes(response, max_bytes).await.map(|_| ())
}

/// 与网关 `supports_checkin` 同口径：国际版只有活跃任务这一条「签到」。
///
/// 保留给签名一致的可读性 —— 本文件所有动作都只服务国际版账号。
#[allow(dead_code)]
pub fn supports_checkin(account: &Value) -> bool {
    account.get("provider").and_then(Value::as_str) == Some(Region::Intl.provider_id())
}
