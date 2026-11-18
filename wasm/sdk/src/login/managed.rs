//! 任务级托管登录薄封装。没有本地监听器/浏览器实现，所有权限来自当前可信宿主 Call。
use serde::Deserialize;
use crate::{host, json, Error, Result, Value};

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    pub provider: String,
    pub flow: String,
    pub method_index: u64,
    pub interaction: String,
    pub expires_at: i64,
    pub remaining_ms: u64,
    pub authorization_origins: Vec<String>,
}

pub fn context() -> Result<Context> {
    serde_json::from_value(host("login.context", json!({}))?)
        .map_err(|_| Error::new("宿主托管登录上下文无效"))
}

/// 只在可信 login.submit / login.callback 中成功一次；不能读取旧票据或替别的 flow 取值。
pub fn take_input() -> Result<Value> { host("login.input.take", json!({})) }

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Loopback {
    pub lease_id: String,
    pub callback_url: String,
    pub expires_at: i64,
}

pub fn open_loopback(profile: &str, expected: &str) -> Result<Loopback> {
    serde_json::from_value(host("login.loopback.open", json!({ "profile": profile, "expected": expected }))?)
        .map_err(|_| Error::new("宿主回调租约无效"))
}

pub fn close_loopback(lease_id: &str) -> Result<bool> {
    Ok(host("login.loopback.close", json!({ "leaseId": lease_id }))?["closed"].as_bool().unwrap_or(false))
}

pub fn publish_captcha(expected: &str, title: &str, message: &str, prefix: &str, scene_id: &str, region: &str) -> Result<Value> {
    host("login.step.publish", json!({ "expected": expected, "title": title, "message": message,
        "captcha": { "engine": "aliyun", "protocol": "v2", "prefix": prefix, "sceneId": scene_id, "region": region },
        "browser": null }))
}

pub fn publish_browser(expected: &str, title: &str, message: &str, url: &str, lease_id: &str) -> Result<Value> {
    host("login.step.publish", json!({ "expected": expected, "title": title, "message": message, "captcha": null,
        "browser": { "url": url, "leaseId": lease_id, "browserMode": "external", "manualCallback": true, "callbackInput": "url" } }))
}
