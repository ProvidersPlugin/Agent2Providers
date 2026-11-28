//! Qoder 国际版 10605 队列状态轮询与 ask/finish 租约释放。
//! 放行判定要求 isQueued == false 且 serviceAvailable != false；放弃或结束时尽最大努力释放占位。
use super::{account, client, cosy, credentials::Credentials, endpoints, errors, util};
use agent2api_wasm_sdk::{self as sdk, http, json, Result, Value};

const FINISH_TIMEOUT_MS: u64 = 3_000;
const STATUS_TIMEOUT_MS: u64 = 15_000;
const MAX_STATUS_FAILURES: u32 = 3;

#[derive(Clone)]
pub struct Lease {
    pub origin: String,
    pub model_key: String,
    pub request_set_id: String,
    pub session_id: String,
    pub request_id: String,
    pub queue_type: Option<String>,
    pub began: i64,
}

/// 尽最大努力通知 Qoder 释放队列占位，3 秒上限，失败不阻断或覆盖对话结果。
pub fn finish(lease: &Lease, credentials: &Credentials) {
    if credentials.user.is_empty() || lease.request_set_id.is_empty() || lease.model_key.is_empty()
    {
        return;
    }
    let elapsed = util::now()
        .ok()
        .map(|now| now.saturating_sub(lease.began).max(0))
        .unwrap_or(0);
    let Ok(inner) = serde_json::to_string(&json!({
        "model_key": lease.model_key,
        "request_set_id": lease.request_set_id,
        "user_id": credentials.user,
        "time_consumed": elapsed,
    })) else {
        return;
    };
    let Ok(raw) = serde_json::to_vec(&json!({
        "payload": inner,
        "encodeVersion": "1",
    })) else {
        return;
    };
    let encoded = cosy::encode(&raw);
    let Ok(mut headers) = cosy::headers(Some(&encoded), endpoints::QUEUE_FINISH, credentials)
    else {
        return;
    };
    headers["Content-Type"] = json!("application/json");
    let Ok(url) = client::url(&lease.origin, endpoints::QUEUE_FINISH) else {
        return;
    };
    if let Ok(response) =
        http::open_raw_timeout("POST", &url, headers, Some(&encoded), FINISH_TIMEOUT_MS)
    {
        let _ = response.bytes(64 * 1024);
    }
}

fn status_path(lease: &Lease) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("requestSetId", &lease.request_set_id);
    query.append_pair("modelKey", &lease.model_key);
    if let Some(kind) = lease
        .queue_type
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        query.append_pair("queueType", kind);
    }
    format!("{}?{}", endpoints::QUEUE_STATUS, query.finish())
}

fn poll_once(
    lease: &Lease,
    credentials: &Credentials,
    timeout_ms: u64,
) -> Result<Option<errors::QueueInfo>> {
    let path = status_path(lease);
    let mut headers = cosy::headers(None, &path, credentials)?;
    headers["X-Request-ID"] = json!(lease.request_id);
    headers["X-Session-ID"] = json!(lease.session_id);
    let url = client::url(&lease.origin, &path)?;
    let response = http::open_raw_timeout(
        "GET",
        &url,
        headers,
        None,
        timeout_ms.clamp(1_000, STATUS_TIMEOUT_MS),
    )
    .map_err(|error| client::transport(error, "队列状态查询"))?;
    if response.status == 404 {
        return Ok(None);
    }
    if !(200..300).contains(&response.status) {
        return Err(sdk::Error::http(response.status, "Qoder 队列状态查询失败"));
    }
    let bytes = response
        .bytes(256 * 1024)
        .map_err(|error| client::transport(error, "队列状态查询"))?;
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Ok(errors::queue_info(&value))
}

/// 在预算内轮询队列状态，返回 `(是否放行可重发, 最新队列信息)`。
pub fn wait(
    lease: &mut Lease,
    initial: &errors::QueueInfo,
    credentials: &Credentials,
    snapshot: &account::Snapshot,
    deadline: i64,
) -> Result<(bool, errors::QueueInfo)> {
    let mut latest = initial.clone();
    let mut failures = 0_u32;
    loop {
        account::unchanged(snapshot)?;
        let now = util::now()?;
        if now >= deadline {
            return Ok((false, latest));
        }
        let remaining = deadline.saturating_sub(now) as u64;
        if let Some(suggested) = latest.wait_seconds() {
            if suggested.saturating_mul(1000) > remaining {
                return Ok((false, latest));
            }
        }
        let pause_ms = latest
            .wait_seconds()
            .unwrap_or(5)
            .clamp(1, 30)
            .saturating_mul(1000);
        if pause_ms >= remaining {
            return Ok((false, latest));
        }
        sdk::sleep(pause_ms)?;
        account::unchanged(snapshot)?;
        let left = deadline.saturating_sub(util::now()?);
        if left <= 1_000 {
            return Ok((false, latest));
        }
        match poll_once(lease, credentials, (left as u64).min(STATUS_TIMEOUT_MS)) {
            Ok(Some(info)) => {
                failures = 0;
                if info.queue_type.is_some() {
                    lease.queue_type = info.queue_type.clone();
                }
                latest = info;
                if latest.ready() {
                    if let Some(pause) = latest.retry_after_seconds.filter(|seconds| *seconds > 0) {
                        let left_ms = deadline.saturating_sub(util::now()?).max(0) as u64;
                        let extra = pause.min(5).saturating_mul(1000);
                        if extra > 0 && extra < left_ms {
                            sdk::sleep(extra)?;
                        }
                    }
                    return Ok((util::now()? < deadline, latest));
                }
                if latest.is_queued == Some(true) || latest.service_available == Some(false) {
                    continue;
                }
                return Ok((false, latest));
            }
            Ok(None) => return Ok((false, latest)),
            Err(error) if util::terminal(&error) => return Err(error),
            Err(_) => {
                failures += 1;
                if failures >= MAX_STATUS_FAILURES {
                    return Ok((false, latest));
                }
                latest.retry_after_seconds = Some(5);
                latest.wait_time = None;
            }
        }
    }
}
