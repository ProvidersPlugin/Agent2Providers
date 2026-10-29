//! 可选 Images 扩展；不改变既有握手或 Chat 协议，旧插件返回 NO_METHOD。

use std::collections::BTreeMap;

use agent2api_provider_protocol::WireProxy;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const CAPABILITIES_METHOD: &str = "images.capabilities";
pub const GENERATE_METHOD: &str = "images.generate";
pub const EDIT_METHOD: &str = "images.edit";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImageRequest {
    pub provider: String,
    pub account: String,
    #[serde(default)]
    pub proxy: Option<WireProxy>,
    pub body: Value,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub wire_model: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageResponseKind {
    Openai,
    Gemini,
    ModelscopeAsync,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImagePlan {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
    pub response: ImageResponseKind,
}

impl ImagePlan {
    pub fn openai(url: String, headers: Vec<(String, String)>, body: Value) -> Self {
        Self {
            url,
            headers,
            body,
            response: ImageResponseKind::Openai,
        }
    }
}
