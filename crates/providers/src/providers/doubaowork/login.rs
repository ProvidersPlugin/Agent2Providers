use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::PluginCtx;
use serde_json::json;

use crate::errors::GatewayError;

#[derive(Clone)]
struct Pending {
    expires: i64,
    processing: bool,
}

fn pending() -> &'static Mutex<BTreeMap<String, Pending>> {
    static PENDING: OnceLock<Mutex<BTreeMap<String, Pending>>> = OnceLock::new();
    PENDING.get_or_init(Mutex::default)
}

pub fn start() -> Result<proto::LoginStart, GatewayError> {
    let flow = crate::ids::random_id("doubaowork_login");
    let now = crate::http::now_ms();
    let mut state = pending()
        .lock()
        .map_err(|_| GatewayError::with_status(500, "登录状态不可用"))?;
    state.retain(|_, pending| pending.expires > now);
    if state.len() >= 32 {
        return Err(GatewayError::with_status(
            429,
            "豆包工作登录流程过多，请稍后重试",
        ));
    }
    state.insert(
        flow.clone(),
        Pending {
            expires: now + 5 * 60 * 1000,
            processing: false,
        },
    );
    Ok(proto::LoginStart {
        flow,
        url: format!("{}/", super::api::BASE_URL),
        instructions:
            "在官方页面完成登录，窗口会自动读取 Cookie 并加入账号；网页面板请粘贴完整 Cookie。"
                .into(),
        poll_interval_ms: 0,
        external_browser: false,
        allowed_hosts: vec![
            "doubao.com".into(),
            "feishu.cn".into(),
            "larksuite.com".into(),
        ],
    })
}

pub fn cancel(flow: &str) {
    if let Ok(mut state) = pending().lock() {
        state.remove(flow);
    }
}

pub async fn callback(
    ctx: &PluginCtx,
    callback: proto::LoginCallback,
) -> Result<proto::LoginPoll, GatewayError> {
    {
        let mut state = pending()
            .lock()
            .map_err(|_| GatewayError::with_status(500, "登录状态不可用"))?;
        let Some(pending) = state
            .get_mut(&callback.flow)
            .filter(|pending| pending.expires > crate::http::now_ms() && !pending.processing)
        else {
            return Err(GatewayError::with_status(
                400,
                "豆包工作登录流程已过期或取消，请重新发起",
            ));
        };
        pending.processing = true;
    }
    let account = super::accounts::normalize(
        ctx,
        proto::NormalizeInput {
            provider: super::PROVIDER_ID.into(),
            source: proto::NormalizeSource::Manual {
                payload: json!({"cookie":callback.code}),
            },
            name: None,
            proxy: None,
        },
    )
    .await;
    let current = pending()
        .lock()
        .map_err(|_| GatewayError::with_status(500, "登录状态不可用"))?
        .remove(&callback.flow)
        .is_some_and(|pending| pending.expires > crate::http::now_ms());
    if !current {
        return Err(GatewayError::with_status(
            400,
            "豆包工作登录已取消，不会保存账号",
        ));
    }
    Ok(proto::LoginPoll::Done { account: account? })
}
