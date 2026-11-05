//! 领取专用的一次性验证请求：界面按需运行官方 SDK，结果只交给等待它的领取。

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use agent2api_provider_protocol::WireProxy;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use super::claim::{self, CaptchaConfig, ClaimablePlan};
use super::region::Region;
use crate::errors::GatewayError;

const CLIENT_TTL: Duration = Duration::from_secs(40);
const VERIFY_TIMEOUT: Duration = Duration::from_secs(50);
const MAX_CLIENTS: usize = 32;
const MAX_PENDING: usize = 32;
const POLL_MS: u64 = 5_000;

struct Client {
    seen_at: Instant,
    ready: bool,
}

struct Request {
    provider: &'static str,
    account: String,
    plan_id: String,
    plan_name: String,
    config: CaptchaConfig,
    expires_at: i64,
    deadline: Instant,
    client_id: Option<String>,
    reply: oneshot::Sender<Result<Verification, String>>,
}

#[derive(Default)]
struct Broker {
    clients: BTreeMap<String, Client>,
    requests: BTreeMap<String, Request>,
}

pub(super) struct Verification {
    pub param: String,
    pub region: String,
}

fn broker() -> &'static Mutex<Broker> {
    static BROKER: OnceLock<Mutex<Broker>> = OnceLock::new();
    BROKER.get_or_init(|| Mutex::new(Broker::default()))
}

fn prune(broker: &mut Broker) {
    let now = Instant::now();
    broker.clients.retain(|_, client| now.duration_since(client.seen_at) < CLIENT_TTL);
    broker.requests.retain(|_, request| request.deadline > now && !request.reply.is_closed());
}

struct RequestGuard(String);

impl Drop for RequestGuard {
    fn drop(&mut self) {
        if let Ok(mut broker) = broker().lock() {
            broker.requests.remove(&self.0);
        }
    }
}

/// 配置的 enabled 管领取，skip_model_request 只管模型调用，不能借来免除领取验证。
pub(super) async fn prepare(
    region: Region,
    account: &str,
    plan: &ClaimablePlan,
    proxy: Option<&WireProxy>,
    deadline: Option<tokio::time::Instant>,
) -> Result<Option<Verification>, GatewayError> {
    let config = claim::captcha_config(region, proxy).await?
        .ok_or_else(|| GatewayError::with_status(502, "领取验证配置不完整，未发送领取请求"))?;
    if !config.enabled {
        return Ok(None);
    }
    // 为领取与回执各留一次上游请求的时间，不额外放大整轮签到的等待上限。
    let budget = deadline.map(|deadline| deadline.saturating_duration_since(tokio::time::Instant::now())
        .saturating_sub(Duration::from_secs(35))).unwrap_or(VERIFY_TIMEOUT).min(VERIFY_TIMEOUT);
    if budget < Duration::from_secs(5) {
        return Err(GatewayError::with_status(428, "本轮剩余时间不足以完成验证，请在套餐明细中重新领取"));
    }
    request(region, account, plan, config, budget).await
        .map(Some)
        .map_err(|message| GatewayError::with_status(428, message))
}

async fn request(
    region: Region,
    account: &str,
    plan: &ClaimablePlan,
    config: CaptchaConfig,
    budget: Duration,
) -> Result<Verification, String> {
    let request_id = crate::ids::random_id("zcode_claim");
    let (reply, receive) = oneshot::channel();
    {
        let mut broker = broker().lock().map_err(|_| "领取验证服务暂不可用")?;
        prune(&mut broker);
        if !broker.clients.values().any(|client| client.ready) {
            return Err("没有可用的验证界面，请打开管理面板后重新领取；若界面正忙，请先完成当前验证".to_string());
        }
        if broker.requests.len() >= MAX_PENDING {
            return Err("已有领取正在等待验证，请稍后重试".to_string());
        }
        broker.requests.insert(request_id.clone(), Request {
            provider: region.provider_id(),
            account: account.to_string(),
            plan_id: plan.plan_id.clone(),
            plan_name: plan.name.clone(),
            config,
            expires_at: crate::http::now_ms() + budget.as_millis() as i64,
            deadline: Instant::now() + budget,
            client_id: None,
            reply,
        });
    }
    let _guard = RequestGuard(request_id);
    match tokio::time::timeout(budget, receive).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("领取验证已中断，请在套餐明细中重新领取".to_string()),
        Err(_) => Err("领取验证等待超时，请在套餐明细中完成验证后领取".to_string()),
    }
}

/// 心跳与领取任务同一个短请求，不查询上游、不读取账号凭证。
pub(super) fn poll(payload: &Value) -> Result<Value, String> {
    let client_id = text(payload, "clientId");
    if client_id.is_empty() || client_id.len() > 128 {
        return Err("验证界面标识不完整".to_string());
    }
    let ready = payload.get("ready").and_then(Value::as_bool) == Some(true);
    let mut broker = broker().lock().map_err(|_| "领取验证服务暂不可用")?;
    prune(&mut broker);
    if !broker.clients.contains_key(client_id) && broker.clients.len() >= MAX_CLIENTS {
        return Err("连接的验证界面过多，请关闭多余面板".to_string());
    }
    broker.clients.insert(client_id.to_string(), Client { seen_at: Instant::now(), ready });
    let owned = broker.requests.iter().find_map(|(id, request)| {
        (request.client_id.as_deref() == Some(client_id)).then(|| id.clone())
    });
    let active = owned.is_some();
    let selected = owned.or_else(|| ready.then(|| {
        broker.requests.iter().find_map(|(id, request)| request.client_id.is_none().then(|| id.clone()))
    }).flatten());
    let mut task = Value::Null;
    if let Some(id) = selected {
        if let Some(request) = broker.requests.get_mut(&id) {
            request.client_id = Some(client_id.to_string());
            task = json!({
                "requestId": id,
                "provider": request.provider,
                "accountId": request.account,
                "planId": request.plan_id,
                "planName": request.plan_name,
                "expiresAt": request.expires_at,
                "remainingMs": request.deadline.saturating_duration_since(Instant::now()).as_millis() as u64,
                "config": {
                    "enabled": true,
                    "prefix": request.config.prefix,
                    "sceneId": request.config.scene_id,
                    "region": request.config.region,
                },
            });
        }
    }
    Ok(json!({ "supported": true, "pollMs": POLL_MS, "active": active, "task": task }))
}

/// 只接受独占领取本任务的面板回传；超时、串账号、串套餐与重复回传均不交给领取。
pub(super) fn complete(payload: &Value) -> Result<Value, String> {
    let mut broker = broker().lock().map_err(|_| "领取验证服务暂不可用")?;
    prune(&mut broker);
    let request_id = text(payload, "requestId");
    let Some(request) = broker.requests.get(request_id) else {
        return Ok(json!({ "accepted": false, "reason": "expired" }));
    };
    if request.client_id.as_deref() != Some(text(payload, "clientId"))
        || request.account != text(payload, "accountId")
        || request.plan_id != text(payload, "planId")
    {
        return Err("验证结果与领取任务不匹配".to_string());
    }
    let result = match text(payload, "status") {
        "verified" => {
            let param = text(payload, "param");
            if param.is_empty() || param.len() > 16_384 || param.contains(['\r', '\n']) {
                return Err("领取验证凭据不完整".to_string());
            }
            Ok(Verification { param: param.to_string(), region: request.config.region.clone() })
        }
        "cancelled" => Err("领取验证已取消，请在套餐明细中重新领取".to_string()),
        "timeout" => Err("领取验证等待超时，请在套餐明细中完成验证后领取".to_string()),
        "failed" => {
            let detail: String = text(payload, "message").chars().take(240).collect();
            Err(if detail.is_empty() { "领取验证未通过，请在套餐明细中重新领取".to_string() }
                else { format!("领取验证未通过：{detail}") })
        }
        _ => return Err("领取验证结果类型无效".to_string()),
    };
    if let Some(client) = broker.clients.get_mut(text(payload, "clientId")) {
        client.ready = result.is_ok();
        client.seen_at = Instant::now();
    }
    let accepted = broker.requests.remove(request_id)
        .is_some_and(|request| request.reply.send(result).is_ok());
    Ok(json!({ "accepted": accepted }))
}

fn text<'a>(payload: &'a Value, key: &str) -> &'a str {
    payload.get(key).and_then(Value::as_str).map(str::trim).unwrap_or("")
}
