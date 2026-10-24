//! ModelScope 余额 / 用量查询：魔粒钱包 + 今日发放 + 今日已用调用数。
//!
//! ── 三个接口（都在**网页站**，cookie 鉴权）───────────────────────
//! ```text
//! GET {网页站}/openapi/v1/magicubes/balance
//!   → {"success":true,"data":{"total_balance":108,"available_balance":106,"frozen_amount":2}}
//!
//! GET {网页站}/openapi/v1/magicubes/transactions?type=EARN&page=1&page_size=50
//!   → {"success":true,"data":{"records":[{"title":"注册并登录","total_amount":200,
//!      "gmt_created":"2026-10-19","expire_at":"2026-10-20 05:32:06", …}, …]}}
//!
//! GET {网页站}/api/v1/inference/rate-limit
//!   → {"Code":200,"Success":true,"Data":{"currentUsagePerDay":72}, …}
//! ```
//! 第一个是**主读数**（魔粒钱包）；第二个是「今天领到多少、什么时候过期」；
//! 第三个是「今天已经用掉多少次调用」。
//!
//! ── 为什么值得多打一个流水接口（本文件的存在理由）──────────────
//! 魔粒**只有 1 天有效期**（流水里 `expire_days: 1`，次日 05:0x 前后回收）。
//! `/magicubes/balance` 只是钱包**快照**：它答得出「现在有多少」，答不出
//! 「今天发了多少」「手里这个数什么时候作废」。少了后两个读数，用户看到 200
//! 魔粒时无从判断这是刚发的（今天花得完）还是昨天的尾款（几小时后归零）。
//! `transactions?type=EARN` 里 `gmt_created` = 发放日（北京日期）、`expire_at`
//! = 到期时刻（北京时间），两者正是缺的那半张图。
//!
//! ── 为什么附加接口失败只降级、不整条失败 ────────────────────
//! 主问题是「我还有多少魔粒」，流水与 `rate-limit` 都是附加读数。让一个附加
//! 接口的抖动把余额整条变成红色失败，用户会以为钱包也读不到了 —— 所以它们
//! 失败只记日志、少给那一行，`raw` 里如实标出没取到（缺键与「上游给了空」
//! 在排障时是两件事）。
//!
//! ── 三个请求并发发出 ────────────────────────────────────────
//! 它们是同一次「看看这个账号今天怎么样」的三个面，彼此没有依赖（都只要令牌），
//! 串行 await 会把一次查询的墙钟时间叠成三段。任一失败都不影响其余两个 ——
//! 并发只是编排，不改变上面「主读数失败才整条失败」的分工。
//!
//! ── 量纲与渲染契约（`credits[]` 里混着两类行，别写混）────────────
//! 前端的 `creditRowsOf` 按 `record` 键分两套读法：
//!   · **额度包行**（无 `record`）：`{name, remain, total?, used?, daily?, expireAt?}`
//!     —— 渲染成「剩 x / y」。⚠️ 带 `daily: true` 时 `expireAt` 会被换成
//!     「每日重置」文案（那是「周期回满」语义），**所以真过期的东西不能标 daily**；
//!   · **流水行**（`record: true`）：`{name, amount, unit, at?, expireAt?}` ——
//!     渲染成带符号金额 + 到期时刻，且恒不进「已用完 / 已过期」折叠组。
//!
//! 本家因此：
//!   · 「可用 / 冻结魔粒」= 额度包行（`remain`，有剩余语义）；
//!   · 「今日发放」= **流水行**（`record: true` + `amount` + `expireAt`）——
//!     魔粒只有 1 天有效期，「今天领到多少、什么时候作废」正是流水要表达的；
//!     照额度包 + `daily` 渲染会被显示成「每日重置」，与「几小时后作废」正好说反；
//!   · 「今日已用（次）」= 额度包行但用 **`used`** 键（不是 `remain`）——
//!     前端 `remain` 优先、缺了才读 `used`；把已用量塞进 `remain` 会显示
//!     「剩 72 次」，语义相反。

use serde_json::{json, Value};

use super::client;
use super::region::Region;
use crate::beijing;

/// 魔粒钱包路径（相对网页站）
pub const BALANCE_PATH: &str = "/openapi/v1/magicubes/balance";

/// 魔粒流水路径（相对网页站）。只取 EARN（发放）—— 消耗明细对「今天还能花多少」
/// 没有增量信息，余额本身已经含了结果。
pub const TRANSACTIONS_PATH: &str = "/openapi/v1/magicubes/transactions";

/// 今日用量路径（相对网页站）
pub const RATE_LIMIT_PATH: &str = "/api/v1/inference/rate-limit";

/// 流水取回条数。
///
/// 上游按 `page_size` 截断（参照实现取 50）：一个账号一天正常只有两三笔发放 +
/// 若干消耗，50 条覆盖面足够，响应体也小。取 EARN 之后更只剩发放那几笔。
const TRANSACTIONS_PAGE_SIZE: u32 = 50;

/// 从一份 JSON 里取一个数字（字段缺失 / 非法时给 None）。
///
/// 上游有把数字序列化成字符串的形态，两种都认。
fn pick_number(data: &Value, key: &str) -> Option<f64> {
    let value = data.get(key)?;
    if let Some(number) = value.as_f64() {
        return Some(number);
    }
    value
        .as_str()
        .and_then(|text| text.trim().parse::<f64>().ok())
}

/// 查询魔粒余额、今日发放与今日用量。
pub async fn query_usage(
    region: Region,
    token: &str,
    account_proxy: Option<&agent2api_provider_protocol::WireProxy>,
    gateway_proxy: &str,
) -> Result<Value, String> {
    // ── 三个请求并发（见模块头：「主读数失败才整条失败」的分工不变）──────
    // 路径先绑成变量：`tokio::join!` 里的 `&format!(…)` 临时值活不过这个表达式。
    let transactions_path =
        format!("{TRANSACTIONS_PATH}?type=EARN&page=1&page_size={TRANSACTIONS_PAGE_SIZE}");
    let (balance_read, transactions_read, rate_limit_read) = tokio::join!(
        client::site_get(region, BALANCE_PATH, token, "魔粒余额查询", account_proxy, gateway_proxy),
        client::site_get(region, &transactions_path, token, "魔粒流水查询", account_proxy, gateway_proxy),
        client::site_get(region, RATE_LIMIT_PATH, token, "今日用量查询", account_proxy, gateway_proxy),
    );

    // ── 主读数：魔粒钱包 ────────────────────────────────────
    let (status, balance_payload) = balance_read?;
    if status == 401 || status == 403 {
        return Err(client::expired_message(region, status, true));
    }
    if !(200..300).contains(&status) {
        return Err(format!("魔粒余额查询返回 HTTP {status}"));
    }
    if !client::succeeded(&balance_payload) {
        let message = client::message_of(&balance_payload);
        return Err(if message.trim().is_empty() {
            format!(
                "魔粒余额查询失败（上游 Code={}）",
                client::code_of(&balance_payload)
            )
        } else {
            format!("魔粒余额查询失败：{message}")
        });
    }
    let wallet = client::data_of(&balance_payload);
    let available = pick_number(&wallet, "available_balance").unwrap_or(0.0);
    let total = pick_number(&wallet, "total_balance");
    let frozen = pick_number(&wallet, "frozen_amount").unwrap_or(0.0);

    // ── 附加读数：今日发放 / 到期时刻（失败只降级，见模块头）────────
    // 原始报文一并留下（`raw.transactions`）：没取到给 `Value::Null` —— 排障时
    // 一眼能分辨「上游给了空清单」与「这次根本没问到」，缺键则分辨不出。
    let (earned_today, expires_at, transactions_raw) = match transactions_read {
        Ok((status, payload)) if (200..300).contains(&status) && client::succeeded(&payload) => {
            let (earned, expire) = sum_earned_today(&client::data_of(&payload));
            (earned, expire, payload)
        }
        Ok((_, payload)) => (None, None, payload),
        Err(error) => {
            agent2api_provider_sdk::log_info!(
                "[Usage] ModelScope 魔粒流水未取到（余额照常返回）：{error}"
            );
            (None, None, Value::Null)
        }
    };

    // ── 附加读数：今日已用调用数（失败只降级，见模块头）──────────
    let (used_today, rate_limit_raw) = match rate_limit_read {
        Ok((status, payload)) if (200..300).contains(&status) && client::succeeded(&payload) => {
            (
                pick_number(&client::data_of(&payload), "currentUsagePerDay"),
                payload,
            )
        }
        Ok((_, payload)) => (None, payload),
        Err(error) => {
            agent2api_provider_sdk::log_info!(
                "[Usage] ModelScope 今日用量未取到（余额照常返回）：{error}"
            );
            (None, Value::Null)
        }
    };

    let mut wallets = vec![json!({
        "type": "magicubes",
        "displayName": "魔粒余额",
        "balance": available,
    })];
    if let Some(total) = total {
        if let Some(object) = wallets.first_mut().and_then(Value::as_object_mut) {
            object.insert("total".to_string(), Value::from(total));
        }
    }
    if let Some(object) = wallets.first_mut().and_then(Value::as_object_mut) {
        object.insert("frozen".to_string(), Value::from(frozen));
    }

    let mut credits = vec![
        json!({ "name": "可用魔粒", "remain": available }),
        json!({ "name": "冻结魔粒", "remain": frozen }),
    ];
    // ── 今日发放：走**流水行**读法（`record: true`），不是额度包行 ──────
    // 两个理由，都来自前端的渲染契约：
    //   1. 额度包行带 `daily: true` 时，前端会把 `expireAt` **换成**「每日重置」
    //      文案（那里日额度是「周期回满」语义）。而魔粒是**真过期**（1 天有效期，
    //      次日回收），照额度包渲染会把「几小时后作废」显示成「明天重置回满」——
    //      正好说反；
    //   2. 流水行本来就是「一笔已发生的进项」：前端渲染成 `+200 魔粒` +
    //      `{time} 到期`，且**恒不进「已用完 / 已过期」折叠组**。
    // 与 `chatglm::balance` 的流水行同一套键（`record/amount/unit/expireAt`），
    // 不新增契约。
    if let Some(earned) = earned_today {
        let mut row = json!({
            "name": "今日发放",
            "record": true,
            "amount": earned,
            "unit": "魔粒",
        });
        if let Some(object) = row.as_object_mut() {
            if let Some(expire_ms) = expires_at {
                object.insert("expireAt".to_string(), Value::from(expire_ms));
            }
        }
        credits.push(row);
    }
    // ── 今日已用：用 `used` 键，不用 `remain` ──────────────────────
    // 前端对额度包行的读法是 `remain` 优先（`剩 {remaining}`），只有缺 `remain`
    // 时才读 `used`（`已用 {used}`）。这里放的是**已用量**，塞进 `remain` 会让
    // 界面显示「剩 72 次」——语义正好相反。
    // 量纲是「次」不是魔粒，所以名字里写明（`daily: true` 在这里是正确的：
    // 调用次数确实每天重置）。
    if let Some(used) = used_today {
        credits.push(json!({ "name": "今日已用（次）", "used": used, "daily": true }));
    }

    let mut document = json!({
        "available": available,
        "unit": "魔粒",
        "wallets": wallets,
        "raw": {
            "balance": balance_payload,
            // 没取到时为 null（如实标出，而不是缺键），见上面的取值处
            "transactions": transactions_raw,
            "rateLimit": rate_limit_raw,
        },
    });
    if let Some(object) = document.as_object_mut() {
        object.insert("credits".to_string(), Value::Array(credits));
    }
    Ok(document)
}

/// 从 EARN 流水中汇总**今日发放**与**最晚到期时刻**（两个可选读数）。
///
/// ── 为什么按「日 + 规则」逐行相加 ─────────────────────────────
/// 上游的流水已经把同一「日 + 规则」聚合成一行（`count` 是笔数、`total_amount`
/// 是该行总额），所以今日发放 = 当日各行 `total_amount` 之和。一天正常只有
/// 两三行（「注册并登录」200 + 「绑定阿里云账号」50 之类），相加即可，不必翻页。
///
/// ── 为什么取**最晚**的到期时刻 ───────────────────────────────
/// 每笔发放各自到期，手里那堆魔粒是「先到期先作废」。给最晚的一个，答的是
/// 「今天最后一点魔粒什么时候没了」—— 这才是驱动「今天得用掉」的那个时刻；
/// 给最早的会把用户吓一跳（那笔可能已经花完了）。
///
/// 日期比较用**北京日期键**（`beijing::today_key`）而不是本地时区：上游的
/// `gmt_created` 就是北京日期，拿机器本地时区去比会在 UTC 容器里整整差一天
/// （见 `crate::beijing` 的模块头）。
fn sum_earned_today(data: &Value) -> (Option<f64>, Option<i64>) {
    let Some(records) = data.get("records").and_then(Value::as_array) else {
        return (None, None);
    };
    let today = beijing::today_key();
    let mut sum = 0.0_f64;
    let mut seen = false;
    let mut latest_ms: Option<i64> = None;
    for record in records {
        // `gmt_created` 是「YYYY-MM-DD」（北京日期）。缺了它就不认这一行 ——
        // 宁可少算一笔，也不要把昨天的尾款算进今天。
        if record.get("gmt_created").and_then(Value::as_str).map(str::trim)
            != Some(today.as_str())
        {
            continue;
        }
        if let Some(amount) = pick_number(record, "total_amount") {
            sum += amount;
            seen = true;
        }
        if let Some(expire_ms) = record
            .get("expire_at")
            .and_then(Value::as_str)
            .and_then(parse_beijing_datetime_ms)
        {
            if latest_ms.map(|old| expire_ms > old) != Some(false) {
                latest_ms = Some(expire_ms);
            }
        }
    }
    (seen.then_some(sum), latest_ms)
}

/// `"YYYY-MM-DD HH:MM:SS"`（**北京时间**，上游 `expire_at` 的形态）→ 毫秒时间戳。
///
/// 上游这个字段不带时区后缀，但语义是北京时间（与 `gmt_created` 的日期口径同源）。
/// 按本地时区解析会让 UTC 容器里的到期时刻偏 8 小时 —— 一个「还剩 3 小时」的
/// 提示会变成「已过期」，与 `crate::beijing` 反复强调的那条纪律相悖。
fn parse_beijing_datetime_ms(text: &str) -> Option<i64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let naive = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").ok()?;
    // 北京时刻 = UTC 时刻 - 8h；用固定偏移换算，不依赖宿主机 tzdata
    let offset = chrono::FixedOffset::east_opt(beijing::OFFSET_SECONDS)?;
    // 固定偏移没有夏令时跳变，`.single()` 实际上恒为 Some；走到 None 也只给 None
    // （少一个读数好过猜一个），本文件零 panic。
    naive
        .and_local_timezone(offset)
        .single()
        .map(|value| value.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::{parse_beijing_datetime_ms, pick_number, sum_earned_today};
    use serde_json::{json, Value};

    #[test]
    fn number_accepts_both_json_number_and_string() {
        let data = json!({ "a": 12.5, "b": "12.5", "c": " 12.5 ", "d": "abc" });
        assert_eq!(pick_number(&data, "a"), Some(12.5));
        assert_eq!(pick_number(&data, "b"), Some(12.5));
        assert_eq!(pick_number(&data, "c"), Some(12.5));
        assert_eq!(pick_number(&data, "d"), None);
        assert_eq!(pick_number(&data, "missing"), None);
    }

    #[test]
    fn beijing_datetime_parses_with_fixed_offset() {
        // 2026-10-20 05:32:06 北京时间 = 2026-10-19 21:32:06 UTC
        let text = "2026-10-20 05:32:06";
        let ms = parse_beijing_datetime_ms(text).expect("应当解析成功");
        let value = chrono::DateTime::from_timestamp_millis(ms).expect("时间戳合法");
        assert_eq!(
            value.with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).expect("合法偏移"))
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
            text
        );
        assert_eq!(parse_beijing_datetime_ms(""), None);
        assert_eq!(parse_beijing_datetime_ms("not-a-time"), None);
    }

    #[test]
    fn only_todays_records_count_and_latest_expiry_wins() {
        let today = crate::beijing::today_key();
        let data: Value = json!({
            "records": [
                { "gmt_created": today, "total_amount": 200, "expire_at": "2030-01-02 05:00:00" },
                { "gmt_created": today, "total_amount": 50,  "expire_at": "2030-01-03 05:00:00" },
                // 昨天的尾款不算进今天
                { "gmt_created": "1999-01-01", "total_amount": 999, "expire_at": "2030-01-05 05:00:00" }
            ]
        });
        let (earned, expire) = sum_earned_today(&data);
        assert_eq!(earned, Some(250.0));
        // 取最晚，不是最早
        let latest = parse_beijing_datetime_ms("2030-01-03 05:00:00");
        assert_eq!(expire, latest);
    }

    #[test]
    fn empty_or_malformed_records_yield_nothing() {
        assert_eq!(sum_earned_today(&json!({})), (None, None));
        assert_eq!(sum_earned_today(&json!({ "records": [] })), (None, None));
        // 只有昨天的记录：两个读数都没有
        let data = json!({ "records": [
            { "gmt_created": "1999-01-01", "total_amount": 100, "expire_at": "2030-01-05 05:00:00" }
        ]});
        assert_eq!(sum_earned_today(&data), (None, None));
    }
}
