//! OfficeAce 模型探测：权限拒绝隐藏，目录与推理端不同步只标记。
//!
//! `81009 / Invalid model` 表示推理端不认识该模型名，不等于账号无权限。
//! 探测目标来自目录，所以保留条目并标记 `upstreamStatus: undeployed`；
//! 下轮探测成功后清除标记。`81004 / 81010 / 81016` 和明确的权限拒绝仍隐藏。
//! 凭据级错误、限流、超时和服务端故障不作为新的模型结论；未取到结论时
//! 保留上一份标记。整轮全隐藏时按凭据问题处理，避免把全部目录误删。
//! 结论只在插件进程内缓存 6 小时。

use std::sync::{Mutex, OnceLock};

use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

use crate::http::send_raw;
use crate::logging;

use super::chat::{basic_authorization, chat_endpoint};
use super::credentials::OfficeAceCredential;

/// 结论的新鲜期：超过它，下一次目录刷新会重新探一轮（6 小时）
const TTL_MS: i64 = 6 * 60 * 60 * 1000;
/// 探测请求的 `max_tokens`（只要一个 bit，见模块头）
const PROBE_MAX_TOKENS: i64 = 1;
/// 并发数（慢模型各要 10~14 秒，串行会把一轮拖到十分钟以上）
const CONCURRENCY: usize = 4;
/// 单个探测请求的超时
const PROBE_TIMEOUT_MS: u64 = 60_000;

/// 判「这个模型上游明确不给」的标记（对响应体做小写子串匹配，见模块头）
const REJECTION_MARKERS: &[&str] = &[
    "81004",
    "81010",
    "81016",
    "insufficient permission",
];
const UNDEPLOYED_MARKERS: &[&str] = &["81009", "invalid model"];

#[derive(Clone, Copy)]
enum ProbeOutcome {
    Available,
    Unknown,
    Hidden,
    Undeployed,
}

#[derive(Default)]
struct ProbeResult {
    hidden: Vec<String>,
    undeployed: Vec<String>,
    unknown: Vec<String>,
}

/// 一次探测的状态（在内存里；落盘只在探测收尾时做一次）
#[derive(Clone)]
struct ProbeState {
    /// 上游明确拒绝的模型名
    hidden: Vec<String>,
    undeployed: Vec<String>,
    /// 最近一次探测完成的时刻（毫秒；0 = 从未探过 ⇒ 不藏任何东西）
    probed_at: i64,
    /// 有没有一轮探测正在跑（防止目录被反复拉时并发探）
    probing: bool,
    /// 结论是哪份账号探出来的（换了账号要重探，不看 TTL）
    account: String,
}

static STATE: OnceLock<Mutex<Option<ProbeState>>> = OnceLock::new();

fn state() -> &'static Mutex<Option<ProbeState>> {
    STATE.get_or_init(|| Mutex::new(None))
}

/// 取状态。持锁期间**不**打日志（见模块头：日志要走同一条通道，而
/// `std::sync::Mutex` 不可重入）。
///
/// ── 为什么没有「从库读回」这一层（迁移到插件时删的）────────────
/// 结论的持久化归网关：插件重启后重探一轮（6 小时 TTL 从零开始），而不是让
/// 插件自己开库 —— 插件没有库句柄，也不该有。
fn with_state<R>(f: impl FnOnce(&mut Option<ProbeState>) -> R) -> R {
    let mutex = state();
    let mut guard = mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

pub fn apply(models: &mut Vec<Value>) {
    let (hidden, undeployed) = with_state(|slot| slot.as_ref()
        .map(|state| (state.hidden.clone(), state.undeployed.clone()))
        .unwrap_or_default());
    models.retain(|model| !is_hidden(&hidden, model.get("id").and_then(Value::as_str).unwrap_or("")));
    for model in models.iter_mut() {
        let id = model.get("id").and_then(Value::as_str).unwrap_or("");
        let marked = is_hidden(&undeployed, id);
        if let Some(fields) = model.as_object_mut() {
            fields.remove("upstreamStatus");
            if marked {
                fields.insert("upstreamStatus".to_string(), Value::String("undeployed".to_string()));
            }
        }
    }
}

fn is_hidden(hidden: &[String], id: &str) -> bool {
    hidden.iter().any(|name| name.eq_ignore_ascii_case(id))
}

/// 目录刷新成功后调用：到了该重探的时候（或 `force`）就起一轮后台探测。
///
/// 顺带把「名字已经不在目录里」的旧结论丢掉（上游下架又上架同一个名字时，
/// 旧结论会一上来就把新名字判死 —— 与参考实现的 `prune` 同一考虑）。
pub fn maybe_spawn(ctx: &PluginCtx, credential: &OfficeAceCredential, models: &[Value], force: bool) {
    if !credential.can_forward() {
        return;
    }
    let ids: Vec<String> = models
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_string))
        .filter(|id| !id.is_empty())
        .collect();
    if ids.is_empty() {
        return;
    }
    let now = logging::now_ms();
    let account = credential.id.clone();
    let go = with_state(|slot| {
        if slot.is_none() {
            *slot = Some(ProbeState {
                hidden: Vec::new(),
                undeployed: Vec::new(),
                probed_at: 0,
                probing: false,
                account: String::new(),
            });
        }
        let Some(state) = slot.as_mut() else {
            return false;
        };
        if state.probing {
            return false;
        }
        if state.account != account {
            state.hidden.clear();
            state.undeployed.clear();
        }
        state.hidden.retain(|name| ids.iter().any(|id| id.eq_ignore_ascii_case(name)));
        state.undeployed.retain(|name| ids.iter().any(|id| id.eq_ignore_ascii_case(name)));
        // TTL 没过、且还是同一个账号 ⇒ 沿用上次结论
        if !force
            && state.probed_at > 0
            && state.account == account
            && now - state.probed_at < TTL_MS
        {
            return false;
        }
        state.probing = true;
        state.account = account.clone();
        true
    });
    if !go {
        return;
    }
    let ctx = ctx.clone();
    let base_url = credential.base_url.clone();
    let app_key = credential.model_app_key.clone();
    let app_secret = credential.model_app_secret.clone();
    // 探测是后台动作（慢模型各要 10~14 秒）：不能阻塞目录刷新的返回。
    // 网关那份走 `spawn_task`（能随进程一起收），插件用 `tokio::spawn`。
    tokio::spawn(async move {
        let result = run_probe(&base_url, &app_key, &app_secret, &ids).await;
        let (hidden, undeployed) = with_state(|slot| {
            let Some(state) = slot.as_mut() else { return (Vec::new(), Vec::new()) };
            state.hidden.retain(|id| is_hidden(&result.unknown, id));
            state.undeployed.retain(|id| is_hidden(&result.unknown, id));
            state.hidden.extend(result.hidden);
            state.undeployed.extend(result.undeployed);
            state.probed_at = logging::now_ms();
            state.probing = false;
            (state.hidden.clone(), state.undeployed.clone())
        });
        logging::log(
            "[OfficeAce]",
            &format!(
                "模型探测完成：目录 {} 个；隐藏 {} 个 [{}]；上游未部署 {} 个 [{}]（保留清单）",
                ids.len(), hidden.len(), hidden.join("、"),
                undeployed.len(), undeployed.join("、"),
            ),
        );
        if let Err(error) = ctx.notify_models_changed(super::ID, &account).await {
            agent2api_provider_sdk::log_warn!("[OfficeAce] 通知网关同步模型探测结果失败：{error}");
        }
    });
}

async fn run_probe(base_url: &str, app_key: &str, app_secret: &str, ids: &[String]) -> ProbeResult {
    use futures::StreamExt;

    let endpoint = chat_endpoint(base_url);
    if endpoint.is_empty() {
        return ProbeResult::default();
    }
    let auth = basic_authorization(app_key, app_secret);
    let results = futures::stream::iter(ids.iter().cloned().map(|id| {
        let endpoint = endpoint.clone();
        let auth = auth.clone();
        async move {
            let outcome = probe_one(&endpoint, &auth, &id).await;
            (id, outcome)
        }
    }))
    .buffer_unordered(CONCURRENCY)
    .collect::<Vec<_>>()
    .await;
    let mut result = ProbeResult::default();
    for (id, outcome) in results.iter() {
        match outcome {
            ProbeOutcome::Hidden => result.hidden.push(id.clone()),
            ProbeOutcome::Undeployed => result.undeployed.push(id.clone()),
            ProbeOutcome::Unknown => result.unknown.push(id.clone()),
            ProbeOutcome::Available => {}
        }
    }
    if round_blames_credentials(results.len(), result.hidden.len()) {
        logging::log(
            "[OfficeAce]",
            &format!(
                "模型可用性探测：{} 个模型全被上游拒绝，判成凭据/网关问题，本轮一个都不隐藏",
                results.len()
            ),
        );
        result.hidden.clear();
    }
    result
}

/// 只用网关 Basic 凭据，与转发同源。
async fn probe_one(endpoint: &str, auth: &str, model: &str) -> ProbeOutcome {
    let headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "application/json".to_string()),
        ("Authorization".to_string(), auth.to_string()),
    ];
    let body = json!({
        "model": model,
        "max_tokens": PROBE_MAX_TOKENS,
        "messages": [{ "role": "user", "content": "x" }],
    });
    match send_raw(
        "POST",
        endpoint,
        Some(&body),
        &headers,
        None,
        Some(PROBE_TIMEOUT_MS),
    )
    .await
    {
        Ok(response) if response.ok => ProbeOutcome::Available,
        Ok(response) if response.status == 429 || response.status >= 500
            || is_credential_failure(response.status, &response.payload) => ProbeOutcome::Unknown,
        Ok(response) => model_outcome(&response.payload),
        Err(_) => ProbeOutcome::Unknown,
    }
}

/// 这一发失败是不是**凭据级**的（拒的是这对网关凭据，不是这个模型）。
///
/// 文案单独不够用：`insufficient permission` 既是模型级 81004 的原文，也是华为 IAM
/// 权限错误的通用措辞，光读正文会把「凭据不行」误判成「这个模型不行」——而隐藏是门禁，
/// 错藏直接把用户能用的模型关在门外。两条判据都来自实测码表：
///   - HTTP 401 = 网关鉴权失败（`APIG.1009` 那一族走的就是它）；
///   - 响应体带 `apig.` 前缀的码 = 网关侧（鉴权 / 签名 / 配额）错误，与具体模型无关。
fn is_credential_failure(status: u16, payload: &Option<Value>) -> bool {
    if status == 401 {
        return true;
    }
    payload
        .as_ref()
        .map(|value| value.to_string().to_ascii_lowercase())
        .unwrap_or_default()
        .contains("apig.")
}

/// 整轮全拒 ⇒ 判成凭据 / 网关问题，本轮一个都不隐藏（见模块头）。
///
/// 依据是形状而不是文案：所有模型**同时**被拒，最省事的解释是这对凭据失效或网关在拒
/// 所有请求；而逐模型的权限差异只会拒掉一部分（dev 实测一轮是 28 个里拒 17 个）。
/// 空集合回 false —— 一个模型都没探就不该有任何结论。
fn round_blames_credentials(total: usize, rejected: usize) -> bool {
    total > 0 && rejected == total
}

fn model_outcome(payload: &Option<Value>) -> ProbeOutcome {
    let text = payload.as_ref().map(Value::to_string).unwrap_or_default().to_ascii_lowercase();
    if UNDEPLOYED_MARKERS.iter().any(|marker| text.contains(marker)) {
        ProbeOutcome::Undeployed
    } else if REJECTION_MARKERS.iter().any(|marker| text.contains(marker)) {
        ProbeOutcome::Hidden
    } else {
        ProbeOutcome::Unknown
    }
}
