//! 沿用原生魔粒流水证据：GET /openapi/v1/magicubes/transactions 的 data.records。
//! 每页混有 EARN、消费、过期；只汇总当天 EARN，daily_active 才能确认签到。
//! aliyun_bindlogin 计入日常发放金额，但不能单独当作签到完成证明。

use agent2api_wasm_sdk::{json, Error, Result, Value};
use chrono::{NaiveDate, NaiveDateTime};

use super::{
    client::{self, Client},
    clock,
};

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
    fn is_daily(&self) -> bool {
        matches!(self.rule.as_str(), "daily_active" | "aliyun_bindlogin")
    }

    pub fn credit_row(&self) -> Value {
        json!({ "name": self.title, "record": true, "amount": self.amount,
            "unit": "魔粒", "expireAt": self.expires_at })
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
        for grant in self
            .grants
            .iter()
            .filter(|grant| grant.day == day && (!daily_only || grant.is_daily()))
        {
            total += grant.amount?;
        }
        total.is_finite().then_some(total)
    }

    pub fn signed_on(&self, day: &str) -> bool {
        self.grants.iter().any(|grant| {
            grant.day == day
                && grant.rule == "daily_active"
                && grant.amount.is_some_and(|amount| amount > 0.0)
        })
    }

    pub fn daily_increase(&self, before: &Self, day: &str) -> Option<f64> {
        if !self.complete || !before.complete {
            return None;
        }
        let gained = self.total_on(day, true)? - before.total_on(day, true)?;
        (gained.is_finite() && gained > 0.0).then_some(gained)
    }
}

pub fn read(api: &Client, day: &str) -> Result<Transactions> {
    let mut result = Transactions {
        grants: Vec::new(),
        raw: Vec::new(),
        complete: false,
    };
    let mut last_date: Option<String> = None;
    for page in 1..=MAX_PAGES {
        let payload = api.site_json(
            &format!("{PATH}?page={page}&page_size={PAGE_SIZE}"),
            "魔粒发放记录查询",
        )?;
        let data = client::data_of(&payload);
        let records = data["records"]
            .as_array()
            .ok_or_else(|| Error::http(502, "魔粒发放记录的响应格式无法识别"))?;
        if records.len() > PAGE_SIZE {
            return Err(Error::http(502, "魔粒流水分页超出请求上限，未推断完整发放"));
        }
        for record in records {
            let date = date_of(record)
                .ok_or_else(|| Error::http(502, "魔粒流水缺少有效日期，无法确认当日发放"))?;
            if text(record, "type").is_empty()
                || last_date.as_ref().is_some_and(|previous| date > *previous)
            {
                return Err(Error::http(
                    502,
                    "魔粒流水类型或倒序分页异常，未推断今日已读全",
                ));
            }
            last_date = Some(date);
        }
        let reached_previous_day = records
            .iter()
            .any(|record| date_of(record).is_some_and(|date| date.as_str() < day));
        let count = records.len();
        for record in records {
            if text(record, "type") != "EARN" {
                continue;
            }
            let Some(date) = date_of(record) else {
                continue;
            };
            if date != day {
                continue;
            }
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
        if count < PAGE_SIZE || reached_previous_day {
            result.complete = true;
            break;
        }
    }
    result
        .grants
        .sort_by_key(|grant| grant.expires_at.unwrap_or(i64::MAX));
    api.check()?;
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
    let raw = text(record, "gmt_created");
    if let Ok(time) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(
            time.with_timezone(&clock::offset())
                .format("%Y-%m-%d")
                .to_string(),
        );
    }
    let date = raw.split([' ', 'T']).next()?;
    NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .ok()
        .map(|date| date.to_string())
}

fn text_time_ms(text: &str) -> Option<i64> {
    if let Ok(time) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(time.timestamp_millis());
    }
    ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"]
        .iter()
        .find_map(|format| NaiveDateTime::parse_from_str(text, format).ok())?
        .and_local_timezone(clock::offset())
        .single()
        .map(|time| time.timestamp_millis())
}

pub fn amount_text(amount: f64) -> String {
    format!("{amount:.4}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}
