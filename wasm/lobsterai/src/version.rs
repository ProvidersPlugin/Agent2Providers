//! 版本读取 data.value.version；账号状态保留 12 小时缓存，临时共享缓存不含凭据。
//! 宿主 session TTL 最长 15 分钟，因此不能只照抄原生进程内 12 小时缓存。

use super::{account, upstream};
use agent2api_wasm_sdk::{self as sdk, http, json, state::Scope, Value};

pub const FALLBACK: &str = "2026.11.16";
const TTL: i64 = 12 * 60 * 60 * 1000;
const KEY: &str = "client-version";

fn valid(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 80
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-+".contains(&byte))
}

fn cached(value: &Value) -> Option<String> {
    let version = value["version"].as_str().filter(|text| valid(text))?;
    let age = sdk::now().saturating_sub(value["at"].as_i64()?);
    (0..TTL).contains(&age).then(|| version.to_string())
}

pub fn resolve() -> String {
    if let Ok(snapshot) = sdk::session_get(KEY) {
        if let Some(version) = cached(&snapshot) {
            return version;
        }
    }
    let account_state = sdk::account()
        .ok()
        .and_then(|_| sdk::state::get(Scope::Account, KEY, 1).ok());
    if let Some(snapshot) = &account_state {
        if snapshot.compatible {
            if let Some(version) = cached(&snapshot.value) {
                let _ = sdk::session_put(KEY, snapshot.value.clone(), 900_000);
                return version;
            }
        }
    }
    let value = http::open(
        "GET",
        upstream::VERSION_URL,
        upstream::headers(None, None, false),
        None,
    )
    .and_then(upstream::read_json)
    .ok();
    let resolved = value
        .as_ref()
        .map(|value| account::text(&value["data"]["value"], &["version"]))
        .filter(|version| valid(version))
        .or_else(|| {
            account_state
                .as_ref()
                .filter(|snapshot| snapshot.compatible)
                .and_then(|snapshot| snapshot.value["version"].as_str())
                .filter(|text| valid(text))
                .map(str::to_string)
        })
        .unwrap_or_else(|| FALLBACK.to_string());
    let record = json!({ "version": resolved, "at": sdk::now() });
    let _ = sdk::session_put(KEY, record.clone(), 900_000);
    if let Some(snapshot) = account_state.filter(|snapshot| snapshot.compatible) {
        let _ = sdk::state::compare_exchange(Scope::Account, KEY, 1, snapshot.revision, record, 0);
    }
    resolved
}
