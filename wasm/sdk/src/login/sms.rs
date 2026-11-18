use serde::{Deserialize, Serialize};
use crate::{json, Value};

pub const CAPABILITY: &str = "login.sms.v1";
pub const SEND: &str = "login.sms.send";
pub const VERIFY: &str = "login.sms.verify";

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub phone_label: String,
    pub phone_placeholder: String,
    pub country_codes: Vec<CountryCode>,
    pub phone_min_length: usize,
    pub phone_max_length: usize,
    pub code_min_length: usize,
    pub code_max_length: usize,
    pub resend_after_seconds: u64,
    pub expires_after_seconds: u64,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct CountryCode { pub value: String, pub label: String }

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub method_index: u64,
    pub phone: String,
    pub country_code: String,
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub ticket: Value,
    #[serde(default)]
    pub captcha: Value,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Challenge {
    Geetest { gt: String, challenge: String, success: bool, new_captcha: bool },
    External { url: String },
}

pub fn method(index: u64, label: &str, config: Config) -> Value {
    json!({ "methodIndex": index, "type": "sms", "label": label, "prompts": [],
        "browserMode": "none", "sms": config })
}

/// Ticket is private to the plugin and host; it is never rendered or merged into user input.
pub fn sent(ticket: Value, retry_after_seconds: u64, expires_after_seconds: u64) -> Value {
    json!({ "status": "sent", "ticket": ticket, "retryAfterSeconds": retry_after_seconds,
        "expiresAfterSeconds": expires_after_seconds })
}

pub fn captcha(ticket: Value, challenge: Challenge) -> Value {
    json!({ "status": "captcha", "ticket": ticket, "challenge": challenge })
}
