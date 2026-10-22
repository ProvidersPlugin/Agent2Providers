//! Trae SOLO 通道的请求头集合。
//!
//! ── 为什么把常量写死在这里而不是做成配置 ──────────────────
//! 这一批头不是"客户端偏好"，而是**上游用来选模型表和判血统的指纹**：
//! `X-Ide-Version-Code` 决定 `get_detail_param` 返回哪张目录表
//! （`20260716` 给 35 个、没有 glm-5.3；`20260820` 给 36 个、有），
//! 而"表里没有的模型"回的不是 404，是 HTTP 200 + 流内 4001。
//! 所以**头与目录必须同版本**：改了头不改目录缓存，就会出现
//! "广告出来的模型全 4001"。这两件事都写在本文件，就是为了别改一半。
//!
//! 取值来自参考实现 `plugins/trae/upstream/{constants,headers}.go`
//! （2026-09-03 真实客户端抓包），由 `vectors/trae-vectors.json` 的
//! `headers` 段逐头钉住。

use std::collections::BTreeMap;

pub const IDE_VERSION: &str = "0.1.61";
/// ⚠️ 与模型目录表绑定，见模块头。
pub const IDE_VERSION_CODE: &str = "20260820";
pub const APP_ID: &str = "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8";
pub const DEVICE_BRAND: &str = "83DG";
pub const OS_VERSION: &str = "Windows 11 Pro";
pub const CLIENT_USER_AGENT: &str = "Trae/0.1.61";

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
        if stream { "text/event-stream" } else { "application/json" }.to_string(),
    );
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    headers.insert("User-Agent".to_string(), CLIENT_USER_AGENT.to_string());
    headers.insert("Authorization".to_string(), format!("Cloud-IDE-JWT {}", identity.access_token));
    headers.insert("X-Cloudide-Token".to_string(), identity.access_token.to_string());
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
    headers.insert("X-App-Version".to_string(), "default".to_string());
    headers.insert("X-Ide-Version".to_string(), IDE_VERSION.to_string());
    headers.insert("X-Ide-Version-Code".to_string(), IDE_VERSION_CODE.to_string());
    headers.insert("X-App-Version-Code".to_string(), IDE_VERSION_CODE.to_string());
    headers.insert("X-Ide-Version-Type".to_string(), "stable".to_string());
    headers.insert("X-Device-Type".to_string(), "windows".to_string());
    headers.insert("X-OS-Version".to_string(), OS_VERSION.to_string());
    headers.insert("X-Device-Brand".to_string(), DEVICE_BRAND.to_string());
    headers.insert("Request-Traffic-Type".to_string(), "prod".to_string());
    headers
}

