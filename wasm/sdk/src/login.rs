use serde::{Deserialize, Serialize};
use crate::{json, Value};

#[derive(Clone, Deserialize, Serialize)]
pub struct Field {
    pub key: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub secret: bool,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub options: Vec<OptionItem>,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct OptionItem { pub value: String, pub label: String }
#[derive(Clone, Deserialize, Serialize)]
pub struct Geetest { pub gt: String, pub challenge: String, pub success: bool, pub new_captcha: bool }
#[derive(Clone, Deserialize, Serialize)]
pub struct Interaction {
    pub id: String,
    pub title: String,
    pub message: String,
    pub fields: Vec<Field>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captcha: Option<Geetest>,
}

pub fn start(flow: &str, interaction: Interaction) -> Value {
    json!({ "flow": flow, "url": "", "poll_interval_ms": 1500, "instructions": interaction.message,
        "interaction": interaction })
}
pub fn pending(interaction: Option<Interaction>) -> Value { json!({ "state": "pending", "interaction": interaction }) }
pub fn done(account: Value) -> Value { json!({ "state": "done", "account": account }) }
