//! Trae 目录与聊天请求使用同一套客户端版本头。
//!
//! 上游按版本提供模型目录，不能只更新聊天版本而让目录沿用另一套头。
//! 这里沿用参考项目 magpie Trae 适配所记录的 0.1.69 / 20260917 组合；
//! 账号的 uid / machineId / deviceId 保持登录时的值。

use std::collections::BTreeMap;

pub const IDE_VERSION: &str = "0.1.69";
/// 与模型目录表绑定。
pub const IDE_VERSION_CODE: &str = "20260917";
pub const APP_ID: &str = "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8";
pub const DEVICE_BRAND: &str = "83DG";
pub const OS_VERSION: &str = "Windows 11 Pro";
pub const CLIENT_USER_AGENT: &str = "Trae/0.1.69";

/// 一份凭据里与头有关的四个值（其余字段不参与组头）。
#[derive(Clone, Debug, Default)]
pub struct HeaderIdentity<'a> {
    pub access_token: &'a str,
    pub uid: &'a str,
    pub machine_id: &'a str,
    pub device_id: &'a str,
}

/// `llm_utils_chat` / `get_detail_param` 要的头集合。
///
/// 三条容易漏的：
/// * 同一个 accessToken 要出现在**三个头**里（`Authorization` 带
///   `Cloud-IDE-JWT ` 前缀、`X-Cloudide-Token` 与 `X-Ide-Token` 是裸串）；
/// * `X-Uid` / `X-Machine-Id` / `X-Device-Id` **为空就不发这个头**，
///   发一个空值头上游会当成"另一个身份"；
/// * `Accept` 随流式与否变（`text/event-stream` / `application/json`）。
///
/// 返回 `BTreeMap` 只为测试好比对；出站时由调用方写进 `HeaderMap`。
pub fn solo_headers(identity: &HeaderIdentity<'_>, stream: bool) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    headers.insert(
        "Accept".to_string(),
        if stream {
            "text/event-stream"
        } else {
            "application/json"
        }
        .to_string(),
    );
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    headers.insert("User-Agent".to_string(), CLIENT_USER_AGENT.to_string());
    headers.insert(
        "Authorization".to_string(),
        format!("Cloud-IDE-JWT {}", identity.access_token),
    );
    headers.insert(
        "X-Cloudide-Token".to_string(),
        identity.access_token.to_string(),
    );
    headers.insert("X-Ide-Token".to_string(), identity.access_token.to_string());
    if !identity.uid.is_empty() {
        headers.insert("X-Uid".to_string(), identity.uid.to_string());
    }
    if !identity.machine_id.is_empty() {
        headers.insert("X-Machine-Id".to_string(), identity.machine_id.to_string());
    }
    if !identity.device_id.is_empty() {
        headers.insert("X-Device-Id".to_string(), identity.device_id.to_string());
    }
    headers.insert("X-App-Id".to_string(), APP_ID.to_string());
    headers.insert("X-App-Version".to_string(), IDE_VERSION.to_string());
    headers.insert("X-Ide-Version".to_string(), IDE_VERSION.to_string());
    headers.insert(
        "X-Ide-Version-Code".to_string(),
        IDE_VERSION_CODE.to_string(),
    );
    headers.insert(
        "X-App-Version-Code".to_string(),
        IDE_VERSION_CODE.to_string(),
    );
    headers.insert("X-Ide-Version-Type".to_string(), "stable".to_string());
    headers.insert("X-Device-Type".to_string(), "windows".to_string());
    headers.insert("X-OS-Version".to_string(), OS_VERSION.to_string());
    headers.insert("X-Device-Brand".to_string(), DEVICE_BRAND.to_string());
    if let Ok(request_id) = super::oauth::uuid_v4() {
        headers.insert("X-Request-Id".to_string(), request_id.clone());
        headers.insert("X-Trae-Request-Id".to_string(), request_id);
    }
    headers.insert("X-Plugin-Channel".to_string(), "icube-ai".to_string());
    headers.insert("Request-Traffic-Type".to_string(), "prod".to_string());
    headers
}
