use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use agent2api_provider_protocol as proto;
use serde_json::Value;

use super::{accounts, credentials::SESSION_TTL_MS, login, region::Region};

struct Pending {
    region: Region,
    expires: i64,
    processing: bool,
}

fn pending() -> &'static Mutex<BTreeMap<String, Pending>> {
    static PENDING: OnceLock<Mutex<BTreeMap<String, Pending>>> = OnceLock::new();
    PENDING.get_or_init(Mutex::default)
}

pub(super) fn start(region: Region) -> Result<proto::LoginStart, String> {
    let flow = crate::ids::random_id("monkeycode_login");
    let now = crate::http::now_ms();
    let mut state = pending().lock().map_err(|_| "MonkeyCode 登录状态不可用")?;
    state.retain(|_, item| item.expires > now);
    if state.len() >= 32 {
        return Err("MonkeyCode 登录流程过多，请稍后重试".into());
    }
    state.insert(
        flow.clone(),
        Pending {
            region,
            expires: now + 5 * 60 * 1000,
            processing: false,
        },
    );
    Ok(proto::LoginStart {
        flow,
        url: format!("{}/login", region.site_origin()),
        instructions: "请在桌面内嵌窗口完成官方登录和验证码，成功后自动添加账号；浏览器面板请改用粘贴 session。".into(),
        poll_interval_ms: 0,
        external_browser: false,
        allowed_hosts: region.login_hosts().iter().map(|host| (*host).into()).collect(),
    })
}

pub(super) fn cancel(region: Region, flow: &str) {
    if let Ok(mut state) = pending().lock() {
        if state.get(flow).is_some_and(|item| item.region == region) {
            state.remove(flow);
        }
    }
}

pub(super) async fn callback(
    region: Region,
    callback: proto::LoginCallback,
) -> Result<proto::LoginPoll, String> {
    {
        let mut state = pending().lock().map_err(|_| "MonkeyCode 登录状态不可用")?;
        let Some(item) = state.get_mut(&callback.flow).filter(|item| {
            item.region == region && item.expires > crate::http::now_ms() && !item.processing
        }) else {
            return Err("MonkeyCode 登录流程已过期、取消或与所选站点不符，请重新发起".into());
        };
        item.processing = true;
    }
    let result = normalize_callback(region, &callback.code).await;
    let current = pending()
        .lock()
        .map_err(|_| "MonkeyCode 登录状态不可用")?
        .remove(&callback.flow)
        .is_some_and(|item| item.expires > crate::http::now_ms());
    if !current {
        return Err("MonkeyCode 登录已取消或过期，不会保存账号".into());
    }
    Ok(proto::LoginPoll::Done { account: result? })
}

async fn normalize_callback(
    region: Region,
    code: &str,
) -> Result<proto::NormalizedAccount, String> {
    if code.len() > 16 * 1024 {
        return Err("MonkeyCode 登录凭证过长，请重新登录".into());
    }
    let payload: Value = serde_json::from_str(code).map_err(|_| "MonkeyCode 登录凭证格式无效")?;
    let session = payload
        .get("session")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or("MonkeyCode 登录凭证缺少 session")?;
    let now = crate::http::now_ms();
    let expires = payload.get("expiresAt").and_then(Value::as_i64);
    if expires.is_some_and(|expiry| expiry <= now) {
        return Err("MonkeyCode 登录凭证已经过期，请重新登录".into());
    }
    let mut verified = login::verify_session(region, session, None)
        .await
        .map_err(crate::errors::GatewayError::into_control_message)?;
    if let Some(expiry) = expires {
        verified["expiresAt"] = Value::from(expiry.min(now + SESSION_TTL_MS));
    }
    Ok(accounts::account_of(region, &verified, "web", None))
}
