//! 魔粒发放流水。上游按北京日期聚合，签到确认必须有 daily_active 发放记录。
//! 日常领取金额还包含 aliyun_bindlogin，不能把它单独当成签到完成凭据。
//! GET /openapi/v1/magicubes/transactions?page=1&page_size=50 返回 data.records；
//! 同一页可混有消耗和过期记录，不能只凭请求参数认定每行都是发放。

use chrono::{NaiveDate, NaiveDateTime};
use serde_json::{json, Value};

use super::client::{self, ControlClient};
use crate::beijing;

const PATH: &str = "/openapi/v1/magicubes/transactions";
const PAGE_SIZE: usize = 50;
const MAX_PAGES: usize = 10;

pub struct Grant {
    pub day: String,
    pub rule: String,
    pub title: String,
    pub amount: Option<f64>,
    pub expires_at: Option<i64>,
}

impl Grant {
    pub fn is_daily(&self) -> bool {
        matches!(self.rule.as_str(), "daily_active" | "aliyun_bindlogin")
    }

    pub fn credit_row(&self) -> Value {
        json!({
            "name": self.title,
            "record": true,
            "amount": self.amount,
            "unit": "魔粒",
            "expireAt": self.expires_at,
        })
    }
}

pub struct Transactions {
    pub grants: Vec<Grant>,
    pub raw: Vec<Value>,
    pub complete: bool,
}

impl Transactions {
    pub fn total_on(&self, day: &str, daily_only: bool) -> Option<f64> {
        let mut total = 0.0;
        for grant in self.grants.iter().filter(|grant| {
            grant.day == day && (!daily_only || grant.is_daily())
        }) {
            total += grant.amount?;
        }
        total.is_finite().then_some(total)
    }

    pub fn signed_on(&self, day: &str) -> bool {
        self.grants.iter().any(|grant| {
            grant.day == day && grant.rule == "daily_active" && grant.amount.is_some_and(|n| n > 0.0)
        })
    }

    pub fn daily_increase(&self, before: &Self, day: &str) -> Option<f64> {
        if !self.complete || !before.complete { return None; }
        let gained = self.total_on(day, true)? - before.total_on(day, true)?;
        (gained.is_finite() && gained > 0.0).then_some(gained)
    }
}

pub async fn read(api: &ControlClient, day: &str) -> Result<Transactions, String> {
    let mut result = Transactions { grants: Vec::new(), raw: Vec::new(), complete: false };
    for page in 1..=MAX_PAGES {
        let payload = api.site_json(
            &format!("{PATH}?page={page}&page_size={PAGE_SIZE}"),
            "魔粒发放记录查询",
        ).await?;
        let data = client::data_of(&payload);
        let records = match data.get("records") {
            Some(Value::Array(records)) => records.as_slice(),
            _ => return Err("魔粒发放记录的响应格式无法识别".to_string()),
        };
        for record in records {
            if date_of(record).is_none() || text(record, "type").is_empty() {
                return Err("魔粒流水缺少有效的日期或类型，无法确认当日发放".to_string());
            }
        }
        let reached_previous_day = records.iter().any(|record| {
            date_of(record).is_some_and(|date| date.as_str() < day)
        });
        let count = records.len();
        for record in records {
            if text(record, "type") != "EARN" { continue; }
            let Some(date) = date_of(record) else { continue; };
            if date != day { continue; }
            result.grants.push(Grant {
                day: date,
                rule: text(record, "rule_key").to_string(),
                title: match text(record, "title") {
                    "" => "魔粒发放".to_string(),
                    title => title.to_string(),
                },
                amount: number(record, "total_amount").filter(|amount| *amount >= 0.0),
                expires_at: text_time_ms(text(record, "expire_at")),
            });
        }
        result.raw.push(payload);
        // 流水按日期倒序；到达更早日期后，当日记录已读全。
        if count < PAGE_SIZE || reached_previous_day {
            result.complete = true;
            break;
        }
    }
    result.grants.sort_by_key(|grant| grant.expires_at.unwrap_or(i64::MAX));
    Ok(result)
}

pub fn number(value: &Value, key: &str) -> Option<f64> {
    let raw = value.get(key)?;
    raw.as_f64()
        .or_else(|| raw.as_str()?.trim().parse().ok())
        .filter(|number| number.is_finite())
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("").trim()
}

fn date_of(record: &Value) -> Option<String> {
    let text = text(record, "gmt_created");
    let date = text.split([' ', 'T']).next()?;
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok().map(|date| date.to_string())
}

fn text_time_ms(text: &str) -> Option<i64> {
    NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").ok()?
        .and_local_timezone(beijing::offset()).single()
        .map(|time| time.timestamp_millis())
}

pub fn amount_text(amount: f64) -> String {
    format!("{amount:.4}").trim_end_matches('0').trim_end_matches('.').to_string()
}
