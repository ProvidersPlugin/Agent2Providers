//! 计费域的**运营动作**：签到状态 / 领取 / 一次性奖励 / 运营 banner。
//!
//! ── 这些原来是网关的 HTTP 端点 ───────────────────────────────
//! ```text
//!   GET  /api/usage                    → usage.query（已随 usage.rs 搬入）
//!   GET  /api/checkin/status           → action = "billing.checkin-status"
//!   POST /api/checkin                  → action = "billing.checkin-claim"
//!   POST /api/checkin/claim-and-report → 网关组合（status + claim + usage）
//!   GET  /api/activity/banner          → action = "billing.banner"
//!   GET  /api/activity/ambassador      → action = "billing.ambassador"
//!   （签到末尾的一次性奖励）              → action = "billing.reward"
//! ```
//!
//! 返回形状**逐字保留**：前端与签到台账读的就是这些键，形状一变界面就白屏
//! 或「今日已签」不再点亮。
//!
//! ── 为什么这些必须搬 ────────────────────────────────────────
//! 端点路径（`/v2/billing/meter/daily-checkin`）、重复领取的上游文案
//! （「今天已签到，请明天再来」）、跨站落点（`www.codebuddy.cn`）与商品码
//! 都是「这家上游长什么样」的知识。按项目的风险隔离要求，这类实现不留在
//! 主仓库。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Map, Value};

use crate::errors::GatewayError;
use crate::workbuddy_shared::endpoints::RESPONSE_CODE_OK;

use super::call::{self, BillingCall, Options};
use super::checkin::normalize_checkin;
use super::tools::js_truthy;

use super::super::Region;

/// 国内版签到状态端点（带 `/v2`）。
const CHECKIN_STATUS_PATH: &str = "/v2/billing/meter/checkin-activity-status";
/// 国内版每日签到领取端点。
const DAILY_CHECKIN_PATH: &str = "/v2/billing/meter/daily-checkin";
/// 国际版的活跃探测端点：**不带 `/v2`**（官方客户端就是这么调的）。
const ACTIVITY_STATUS_PATH: &str = "/billing/meter/checkin-activity-status";
/// 运营 banner（需要客户端白名单头，否则被拦截）。
const ACTIVITY_BANNER_PATH: &str = "/v2/activity/workbuddy/banner";
/// 大使状态。
const ACTIVITY_AMBASSADOR_PATH: &str = "/v2/activity/ambassador/status";
/// 一次性奖励的固定落点：国内版计费站（上游运营侧固定，不跟账号端点走）。
const REWARD_BASE: &str = "https://www.codebuddy.cn";

/// 签到状态。
///
/// 国内版有签到活动接口；国际版走「每日活跃」探测，响应归一成同一套键
/// （`active` / `todayCheckedIn`），调用方不用分版本读字段。
/// 成功但 `data` 为空时返回 `Null`（与桌面端一致）。
pub(crate) async fn checkin_status(
    session: &Value,
    region: Region,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
) -> Result<Value, GatewayError> {
    if region == Region::Intl {
        let result = call::call(
            "POST",
            ACTIVITY_STATUS_PATH,
            None,
            session,
            proxy,
            None,
            Options {
                expect_code_ok: false,
                ..Default::default()
            },
        )
        .await?;
        return Ok(normalize_activity_status(&result));
    }
    let result = call::call(
        "POST",
        CHECKIN_STATUS_PATH,
        None,
        session,
        proxy,
        None,
        Options {
            expect_code_ok: false,
            ..Default::default()
        },
    )
    .await?;
    if result.code != Some(RESPONSE_CODE_OK) || result.data.is_null() {
        return Ok(Value::Null);
    }
    Ok(normalize_checkin(&result.data))
}

/// 领取每日签到（国内版）。
///
/// 幂等：已领取时上游返回非 0 code（或 HTTP 400），这里原样返回
/// `{success:false, code, msg}` ——「今天已签到」不是错误，前端面板把它显示成
/// 一条 warn 提示。
///
/// 国际版没有「领取签到」这个动作（走每日活跃链，见 `keepalive`），
/// 被调到就明确报错而不是打一条注定失败的国内版接口。
pub(crate) async fn claim_daily(
    session: &Value,
    region: Region,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
) -> Result<Value, GatewayError> {
    if region == Region::Intl {
        return Err(GatewayError::with_status(400, "国际版账号暂无签到活动"));
    }
    let result = call::call(
        "POST",
        DAILY_CHECKIN_PATH,
        None,
        session,
        proxy,
        None,
        Options {
            expect_code_ok: false,
            tolerate_duplicate_claim: true,
            ..Default::default()
        },
    )
    .await?;
    Ok(normalize_daily_claim(result))
}

/// 一次性运营奖励（`claim-gift` / `claim-compensation`）。
///
/// 国内版专属：落点固定在 `www.codebuddy.cn` 的计费站。每号一次 ——
/// 「已领取」与「没有可领取的」都是正常终态，必须原样返回而不是丢成 `Err`：
///   · HTTP 200 + 非 0 code：`expect_code_ok=false` 原样解包；
///   · HTTP 400/409 + 「已领取」文案：`tolerate_duplicate_claim` 放行
///     （与签到那条共用一份判据，见 `call::is_duplicate_claim`）。
///
/// `kind` 只认 `gift` / `compensation`；其余如实报错（静默当成某一个会让
/// 「点了另一个按钮却跑了这套请求」变成查不出来的事）。
pub(crate) async fn claim_reward(
    session: &Value,
    region: Region,
    kind: &str,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
) -> Result<Value, GatewayError> {
    if region == Region::Intl {
        return Err(GatewayError::with_status(
            400,
            "国际版没有一次性运营奖励（新手礼包 / 活动补偿只在国内版）",
        ));
    }
    let (path, label) = match kind {
        "gift" => ("/billing/meter/claim-gift", "新手礼包"),
        "compensation" => ("/billing/meter/claim-compensation", "活动补偿"),
        other => {
            return Err(GatewayError::with_status(
                400,
                format!("WorkBuddy 不认识奖励类型「{other}」（可用：gift / compensation）"),
            ))
        }
    };
    let result = call::call(
        "POST",
        path,
        None,
        session,
        proxy,
        None,
        Options {
            base: Some(REWARD_BASE),
            expect_code_ok: false,
            tolerate_duplicate_claim: true,
            ..Default::default()
        },
    )
    .await?;
    Ok(normalize_reward(result, label))
}

/// 运营 banner。
///
/// **吞掉所有错误并返回 `Null`** —— banner 是页面上的一块装饰，拉不到就不显示，
/// 不该让首屏出现一条红色报错（与桌面端一致）。
pub(crate) async fn banner(
    session: &Value,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
) -> Value {
    let result = match call::call(
        "GET",
        ACTIVITY_BANNER_PATH,
        None,
        session,
        proxy,
        None,
        Options {
            whitelist_headers: true,
            expect_code_ok: false,
            ..Default::default()
        },
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            crate::logging::log("[Activity]", &format!("banner 拉取失败: {}", error.message));
            return Value::Null;
        }
    };
    if result.code != Some(RESPONSE_CODE_OK) || result.data.is_null() {
        return Value::Null;
    }
    let raw = result.data;
    // action 只在 type 是 open_url / switch_model 时透出（其余类型前端不认识）。
    // 键的取舍照抄 Node 的对象字面量：`url` / `model_id` / `fallback_url`
    // 在缺失时是 undefined → **键整个消失**。实测上游的 open_url 就是
    // `{type, label, fallback_url}`（没有 url），所以响应里也不该有 url 键。
    let action = raw.get("action").and_then(|action| {
        let action_type = action.get("type").and_then(Value::as_str).unwrap_or("");
        if action_type != "open_url" && action_type != "switch_model" {
            return None;
        }
        let mut object = Map::new();
        object.insert("type".to_string(), Value::String(action_type.to_string()));
        object.insert(
            "label".to_string(),
            action.get("label").cloned().unwrap_or(Value::Null),
        );
        if let Some(url) = action.get("url") {
            object.insert("url".to_string(), url.clone());
        }
        if let Some(model_id) = action.get("model_id") {
            object.insert("modelId".to_string(), model_id.clone());
        }
        if let Some(fallback) = action.get("fallback_url") {
            object.insert("fallbackUrl".to_string(), fallback.clone());
        }
        Some(Value::Object(object))
    });
    let mut banner = Map::new();
    banner.insert(
        "id".to_string(),
        raw.get("activity_id").cloned().unwrap_or(Value::Null),
    );
    // `raw.activity_online_status && raw.status === 'None'` ——
    // 前者是 JS 真值判定，后者是严格字符串比较
    banner.insert(
        "active".to_string(),
        Value::Bool(
            raw.get("activity_online_status").map(js_truthy).unwrap_or(false)
                && raw.get("status").and_then(Value::as_str) == Some("None"),
        ),
    );
    banner.insert(
        "bannerContent".to_string(),
        raw.get("banner_content").cloned().unwrap_or(Value::Null),
    );
    banner.insert(
        "link".to_string(),
        raw.get("detail_link").cloned().unwrap_or(Value::Null),
    );
    banner.insert(
        "level".to_string(),
        raw.get("activity_level").cloned().unwrap_or(Value::Null),
    );
    banner.insert(
        "startTime".to_string(),
        raw.get("start_time").cloned().unwrap_or(Value::Null),
    );
    banner.insert(
        "endTime".to_string(),
        raw.get("end_time").cloned().unwrap_or(Value::Null),
    );
    if let Some(action) = action {
        banner.insert("action".to_string(), action);
    }
    banner.insert("raw".to_string(), raw);
    Value::Object(banner)
}

/// 大使（推广）状态。同样吞错返回 `Null`（verbose 级别日志）。
pub(crate) async fn ambassador(
    session: &Value,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
) -> Value {
    let result = match call::call(
        "GET",
        ACTIVITY_AMBASSADOR_PATH,
        None,
        session,
        proxy,
        None,
        Options {
            expect_code_ok: false,
            ..Default::default()
        },
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            crate::logging::verbose(
                "[Activity]",
                &format!("ambassador 状态拉取失败: {}", error.message),
            );
            return Value::Null;
        }
    };
    if result.code != Some(RESPONSE_CODE_OK) || result.data.is_null() {
        return Value::Null;
    }
    json!({
        "isAmbassador": result.data.get("isAmbassador").map(js_truthy).unwrap_or(false),
        "raw": result.data,
    })
}

/// 每日领取响应 → 统一 claim 形状（国内版路径）。
///
/// 成功：`{success, code:0, data(归一), raw}`；失败：`{success:false, code, msg,
/// requestId?}` —— 失败分支里 `requestId` 缺失时整个键不出现在 JSON 中
/// （undefined → JSON.stringify 丢键），所以这里也用 Map 组装。
fn normalize_daily_claim(result: BillingCall) -> Value {
    if result.code == Some(RESPONSE_CODE_OK) && !result.data.is_null() {
        return json!({
            "success": true,
            "code": 0,
            "data": normalize_checkin(&result.data),
            "raw": result.data,
        });
    }
    let mut failure = Map::new();
    failure.insert("success".to_string(), Value::Bool(false));
    failure.insert("code".to_string(), Value::from(result.code.unwrap_or(-1)));
    failure.insert(
        "msg".to_string(),
        Value::String(result.msg.unwrap_or_else(|| "签到失败".to_string())),
    );
    if let Some(request_id) = result.request_id {
        failure.insert("requestId".to_string(), request_id);
    }
    Value::Object(failure)
}

/// 一次性奖励领取结果 → 统一 claim 形状。
///
/// 成功：`{success:true, code:0, credit, msg}` —— `credit` 是本次到账积分
/// （缺失按 0），`msg` 带奖励名（「活动补偿 +500 积分」），调用方直接拿它拼日志。
/// 失败 / 已领 / 无可领取同形：`{success:false, code, msg}`。
fn normalize_reward(result: BillingCall, label: &str) -> Value {
    if result.code == Some(RESPONSE_CODE_OK) {
        let credit = result
            .data
            .get("credit")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        return json!({
            "success": true,
            "code": 0,
            "credit": credit,
            "msg": format!("{label} +{credit} 积分"),
        });
    }
    json!({
        "success": false,
        "code": result.code.unwrap_or(-1),
        "msg": result.msg.unwrap_or_else(|| format!("当前没有可领取的{label}")),
    })
}

/// 国际版活跃探测响应 → 管理 API 能直接消费的形态。
///
/// 上游字段是蛇形（`today_checked_in` / `daily_credit`），归一成驼峰与国内版
/// 签到状态的键风格一致；`statusAvailable` 区分「接口说不活动」与「接口本身
/// 没答上」—— 前者是正常状态，后者要留给排查。原始 data 原样带出（`raw`）。
pub(super) fn normalize_activity_status(result: &BillingCall) -> Value {
    let data = &result.data;
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
        "statusAvailable": result.code == Some(RESPONSE_CODE_OK) && !data.is_null(),
        "code": result.code,
        "msg": result.msg,
        "raw": data,
    })
}
