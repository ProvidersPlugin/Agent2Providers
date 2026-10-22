//! CodeArts 网页登录：**loopback 回调 + PKCE + DPoP**，双通道收尾。
//!
//! ── 这条链在八家登录里排第几种形态 ──────────────────────────
//! 与 Accio 同族（「浏览器带你回到本机 HTTP 端口」），但**配对方式不同**，
//! 差别全在上游那一侧：
//!   * Accio 的 `return_url` 由我们给，登录页把里面的 `state` 原样带回 → 按 state 查表；
//!   * CodeArts 的 portal **只认我们给的 `port`**，回调路径由它自己拼成
//!     `http://127.0.0.1:<port>/oauth/callback`，而且**实测可能只带一个 `code`**、
//!     不带任何配对信息。所以不能只按 state 查，得「逐个候选试 verifier」：
//!     用错 verifier 会被 STS 判 `STS5.1805` 而**不消耗授权码**，试到通过为止是安全的。
//!
//! ── 两次回调 ────────────────────────────────────────────────
//! portal 先带 `secret` + `redirect` 回来一次，并要求我们 **307 跳到它给的 redirect**
//! （不跳，链路就停在半路、永远不会有 `code`）；随后再带 `code` 回来。
//! 第一次那趟顺手把 secret 记进待办，作为 ticket 轮询通道的钥匙。
//!
//! ── 为什么还要 ticket 轮询这条兜底 ──────────────────────────
//! 网关跑在另一台机器上（NAS / 服务器）时，浏览器跳的是**它自己**的 127.0.0.1，
//! 两次回调都到不了插件这一侧。此时用户把地址栏里那条 localhost 地址改成网关地址
//! 再走一遍（界面上就是这么教的），粘回来的带的是 `secret` 而不是 `code` ——
//! 只能靠 `GET {base}/snap-manager/v1/login/ticket` 换。注意这条通道**不返回
//! refresh token**，经它落账的账号约一小时后就得重新登录，文案要说清。
//!
//! ── 为什么报 `poll_interval_ms > 0`（本家是回调型）────────────
//! 「轮询型」在协议里的含义是「**网关主动问插件要结果**」，与授权在哪里终结无关。
//! 本家的回调打在**插件自己**的 loopback 端口上，永远不经过网关的
//! `/api/session/login/callback` —— 报 `0` 的话网关会一直挂着等一个不会到来的
//! 回调，没有任何报错。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use agent2api_provider_protocol as proto;

use crate::logging;

use super::callback_server::CallbackListener;
use super::credentials::Credential;
use super::oauth::{self, PendingLogin};
use super::{accounts, chat, models};

/// 流程 id 前缀
const FLOW_PREFIX: &str = "codearts-web-";

/// ticket 通道的轮询间隔（portal 那边用户可能还没点完，这条会连着 404 一阵）。
const TICKET_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// 一轮登录在进程内的状态。
enum FlowState {
    /// 授权地址已给出，等回调。
    Waiting,
    /// 拿到授权码 / secret，正在换证或等 ticket。
    Working,
    /// 换好了（存的是凭据的落库形态）。
    Done(serde_json::Value),
    /// 这一轮没成。
    Failed(String),
}

struct Entry {
    listener: Arc<CallbackListener>,
    /// 这一轮的 ticket（= `oauth` 待办表里的键 = 登录任务的 state）。
    ticket: String,
    state: FlowState,
}

fn flows() -> &'static Mutex<HashMap<String, Entry>> {
    static FLOWS: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    FLOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_flows<T>(action: impl FnOnce(&mut HashMap<String, Entry>) -> T) -> T {
    match flows().lock() {
        Ok(mut table) => action(&mut table),
        Err(poisoned) => action(&mut poisoned.into_inner()),
    }
}

/// 发起一次 CodeArts 网页登录。
pub(super) async fn login_start(
    method: &str,
    _payload: &serde_json::Value,
) -> Result<proto::LoginStart, String> {
    if method != "browser" {
        return Err(
            "CodeArts 的「填写凭据」请直接在账号页粘贴 AK/SK；网页登录请点「网页登录」"
                .to_string(),
        );
    }
    // 上一轮没收摊的先关掉：端口不释放的话，这一轮的回调可能落到上一轮的
    // 监听器上（授权码一次性，两轮并存等于让其中一轮必输）。
    close_all();
    let listener = Arc::new(CallbackListener::bind().await?);
    let callback_url = format!(
        "http://127.0.0.1:{}{}",
        listener.port(),
        super::callback_server::PATH
    );
    let (auth_url, pending) = oauth::begin_login(
        &callback_url,
        chat::DEFAULT_PLUGIN_NAME,
        chat::DEFAULT_PLUGIN_VERSION,
        chat::DEFAULT_LANGUAGE,
    )?;
    let flow_id = format!("{FLOW_PREFIX}{}", pending.ticket_id);
    with_flows(|table| {
        table.insert(
            flow_id.clone(),
            Entry {
                listener: Arc::clone(&listener),
                ticket: pending.ticket_id.clone(),
                state: FlowState::Waiting,
            },
        );
    });
    spawn_completion(flow_id.clone(), Arc::clone(&listener), pending);
    logging::log(
        "[Login]",
        &format!(
            "发起 CodeArts 网页登录（loopback 127.0.0.1:{}，等待浏览器回调…）",
            listener.port()
        ),
    );
    Ok(proto::LoginStart {
        flow: flow_id,
        url: auth_url,
        instructions: "在打开的页面里完成华为云账号登录，完成后会自动收下账号".to_string(),
        // 见模块头：回调打在插件自己的 loopback 端口上，网关必须来问
        poll_interval_ms: 2_000,
        // 内嵌窗口：回调是 `http://127.0.0.1:<port>/oauth/callback`，
        // 同一台机器上内嵌窗口能正常导航过去
        external_browser: false,
        // 空 = 不限制：授权页会继续跳华为云的多个主机
        allowed_hosts: Vec::new(),
    })
}

/// 后台那半个登录：等回调 → 按通道收尾 → 写状态。
///
/// ── 为什么在这里分两条通道 ──────────────────────────────────
/// 回调里带 `code` → 直接换码（正常路径，拿得到 refresh token）；
/// 只带 `secret` → 转 ticket 轮询（拿不到 refresh token，但至少能把账号建起来）。
/// 两条通道**共用**同一个监听器与同一个待办表，不复制第二套换证逻辑。
fn spawn_completion(flow_id: String, listener: Arc<CallbackListener>, pending: PendingLogin) {
    tokio::spawn(async move {
        let query = listener.next_callback().await;
        let Some(query) = query else {
            // 通道关了（用户取消 / 面板超时清理）—— 把状态如实标出来
            with_flows(|table| {
                if let Some(entry) = table.get_mut(&flow_id) {
                    if matches!(entry.state, FlowState::Waiting) {
                        entry.state = FlowState::Failed("这次登录已取消，请重新发起".to_string());
                    }
                }
            });
            return;
        };
        set_state(&flow_id, FlowState::Working);
        let params = parse_query(&query);
        let outcome = finish(&params, &pending).await;
        with_flows(|table| {
            if let Some(entry) = table.get_mut(&flow_id) {
                entry.state = match outcome {
                    Ok(credential) => FlowState::Done(credential.to_value()),
                    Err(message) => FlowState::Failed(message),
                };
            }
        });
        // 这一轮收摊：端口还回去
        listener.close();
    });
}

fn set_state(flow_id: &str, next: FlowState) {
    with_flows(|table| {
        if let Some(entry) = table.get_mut(flow_id) {
            entry.state = next;
        }
    });
}

/// 用回调参数收尾（授权码通道优先，其次 ticket 通道）。
async fn finish(
    params: &HashMap<String, String>,
    pending: &PendingLogin,
) -> Result<Credential, String> {
    let get = |key: &str| {
        params
            .get(key)
            .map(|value| percent_decode(value))
            .unwrap_or_default()
    };
    let code = get("code");
    let secret = get("secret");
    if !code.is_empty() {
        return exchange_by_code(&code, pending).await;
    }
    if !secret.is_empty() {
        // portal 下发的 secret 记进**这一轮**（ticket 通道用的就是它）。
        // 监听器那边已经投递过一次 query，这里再记一次是幂等的。
        oauth::attach_ticket_secret(&pending.ticket_id, &secret);
        return exchange_by_ticket(pending).await;
    }
    Err("回调里没有授权码也没有 secret：请把浏览器地址栏里那条回调地址完整粘贴过来".to_string())
}

/// 授权码通道：**先按本轮试，失败再逐个候选试**。
///
/// ── 为什么可以逐个试 ────────────────────────────────────────
/// 用错的 verifier 会被 STS 判 `STS5.1805` 而**不消耗授权码**（参考实现与
/// 独立实现的注释都写了这一条），所以试到通过为止是安全的。
///
/// ── 为什么用 `peek` 而不是 `take` ───────────────────────────
/// 换码失败时这一轮必须留在表里。免鉴权端口上任何人打一次「随便编个 code」
/// 就能把用户正在进行的登录全部作废 —— 取走式写法等于把「取消别人的登录」
/// 做成免费服务。**只有成功才取走**。
async fn exchange_by_code(code: &str, pending: &PendingLogin) -> Result<Credential, String> {
    let mut ordered: Vec<PendingLogin> = Vec::new();
    let candidates = oauth::candidates();
    // 本轮排第一（端口归属已经确定了这一轮，它最可能是对的）
    ordered.extend(
        candidates
            .iter()
            .filter(|item| item.ticket_id == pending.ticket_id)
            .cloned(),
    );
    for item in candidates
        .iter()
        .filter(|item| item.ticket_id != pending.ticket_id)
    {
        ordered.push(item.clone());
    }
    if ordered.is_empty() {
        return Err(format!(
            "这一轮登录已作废（超过 {} 分钟或已被取消），请回到面板重新发起",
            oauth::LOGIN_TIMEOUT_MS / 60_000
        ));
    }
    let mut last = String::new();
    for candidate in ordered {
        let Some(peeked) = oauth::peek_pending(&candidate.ticket_id) else {
            continue;
        };
        match oauth::exchange_for(&peeked, code, None).await {
            Ok(credential) => {
                // 成功才取走：授权码与 verifier 都是一次性的
                oauth::take_pending(&candidate.ticket_id);
                return Ok(credential);
            }
            Err(error) => {
                logging::log(
                    "[Login]",
                    &format!(
                        "CodeArts 换码候选 {} 未通过：{}",
                        short(&candidate.ticket_id),
                        error.message
                    ),
                );
                last = error.message;
            }
        }
    }
    Err(format!("授权码换取凭证失败：{last}"))
}

/// ticket 通道：每 2 秒问一次，到本轮登录超时为止。
///
/// ── 为什么在这里就地轮询（而不是像网关那样起后台任务）──────────
/// 插件这一侧本来就在一个后台任务里（`spawn_completion`），再套一层任务只是
/// 多一个句柄要管；就地循环的退出条件与那个任务完全一致（超时 / 成功 / 取消
/// 由 `close` 触发的 channel 关闭表达）。
async fn exchange_by_ticket(pending: &PendingLogin) -> Result<Credential, String> {
    let deadline = logging::now_ms() + oauth::LOGIN_TIMEOUT_MS as i64;
    loop {
        // 只读一份来轮：换成功才从表里取走
        let Some(current) = oauth::peek_pending(&pending.ticket_id) else {
            return Err("这一轮登录已取消，请重新发起".to_string());
        };
        if logging::now_ms() >= deadline {
            return Err("登录超时：ticket 通道没能换取到凭证，请重新发起".to_string());
        }
        match oauth::poll_ticket(models::DEFAULT_BASE_URL, &current, None).await {
            Ok(mut credential) => {
                let Some(_claimed) = oauth::take_pending(&pending.ticket_id) else {
                    return Err("这一轮登录已取消，请重新发起".to_string());
                };
                // ticket 通道不返回 refresh token / 上下文，这里把**本轮**生成的
                // PKCE 与 DPoP 补进去：虽然上游没给 refresh token 因而续不了，
                // 但上下文与凭据同源，留着它至少不会让「凭据形状」看起来缺半块。
                credential.oauth_context = Some(current.context.clone());
                logging::log(
                    "[Login]",
                    "CodeArts 经 ticket 通道落账（该通道不带 refresh token，约一小时后需重新登录）",
                );
                return Ok(credential);
            }
            Err(error) => {
                // 408 是这条链的常态（portal 那边还没就绪），其余才值得记一句
                if error.status_code != 408 {
                    logging::log("[Login]", &format!("CodeArts ticket 通道：{}", error.message));
                }
            }
        }
        tokio::time::sleep(TICKET_POLL_INTERVAL).await;
    }
}

/// 轮询一次登录进度。
pub(super) async fn login_poll(flow: &str) -> Result<proto::LoginPoll, String> {
    let state = with_flows(|table| match table.get(flow) {
        Some(entry) => Some(match &entry.state {
            FlowState::Waiting => proto::LoginPoll::Pending {
                hint: Some("等待你在浏览器里完成授权…".to_string()),
            },
            FlowState::Working => proto::LoginPoll::Pending {
                hint: Some("已收到授权信息，正在换取凭证…".to_string()),
            },
            FlowState::Done(payload) => done_poll(payload),
            FlowState::Failed(message) => proto::LoginPoll::Failed {
                message: message.clone(),
            },
        }),
        None => None,
    });
    match state {
        Some(result) => {
            let terminal = matches!(
                result,
                proto::LoginPoll::Done { .. } | proto::LoginPoll::Failed { .. }
            );
            if terminal {
                let ticket = with_flows(|table| table.remove(flow).map(|entry| entry.ticket));
                if let Some(ticket) = ticket {
                    oauth::drop_pending(&ticket);
                }
            }
            Ok(result)
        }
        None => Ok(proto::LoginPoll::Failed {
            message: "这次登录流程已失效（插件进程重启过？），请重新发起".to_string(),
        }),
    }
}

/// 用户粘贴回来的回调 URL。
///
/// ── 这条为什么存在 ──────────────────────────────────────────
/// 授权页在本机跑时回调能直达 loopback；但如果用户是在**另一台机器**上完成
/// 授权（远程浏览器 / 容器里跑网关），回调就落不到这一台上 —— 那时用户会
/// 把浏览器地址栏里那条 URL 粘回来。两条路共用同一个监听器（投递后由后台
/// 任务照常换证），不复制第二套换证逻辑。
pub(super) async fn login_callback(
    callback: proto::LoginCallback,
) -> Result<proto::LoginPoll, String> {
    let query = callback_query(&callback);
    let listener = with_flows(|table| {
        table.get(&callback.flow).map(|entry| {
            entry.listener.submit_callback(&query)?;
            Ok::<_, String>(Arc::clone(&entry.listener))
        })
    });
    let Some(listener) = listener else {
        return Ok(proto::LoginPoll::Failed {
            message: "这次登录流程已失效，请重新发起".to_string(),
        });
    };
    let _listener = listener?;
    // 后台任务已经在等回调，投递之后它会立刻进入换证。这里只等状态翻转，
    // 不自己再消费一次 channel（两个 consumer 抢同一个 receiver 只会让
    // 其中一边永远等不到）。
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let state = with_flows(|table| match table.get(&callback.flow) {
            Some(entry) => match &entry.state {
                FlowState::Done(payload) => Some(done_poll(payload)),
                FlowState::Failed(message) => Some(proto::LoginPoll::Failed {
                    message: message.clone(),
                }),
                _ => None,
            },
            None => Some(proto::LoginPoll::Failed {
                message: "这次登录流程已失效，请重新发起".to_string(),
            }),
        });
        if let Some(result) = state {
            let terminal = matches!(
                result,
                proto::LoginPoll::Done { .. } | proto::LoginPoll::Failed { .. }
            );
            if terminal {
                with_flows(|table| table.remove(&callback.flow));
            }
            return Ok(result);
        }
    }
    // 30 秒还没换完：如实说「还在换」
    Ok(proto::LoginPoll::Pending {
        hint: Some("已收到授权信息，仍在换取凭证…".to_string()),
    })
}

/// 取消一轮登录：收监听器（释放端口）并清掉那一条。
pub(super) async fn login_cancel(flow: &str) -> Result<(), String> {
    let removed = with_flows(|table| table.remove(flow));
    if let Some(entry) = removed {
        entry.listener.close();
        oauth::drop_pending(&entry.ticket);
    }
    Ok(())
}

/// 收掉表里所有轮次（发起新登录前调）。
fn close_all() {
    let drained: Vec<Entry> = with_flows(|table| table.drain().map(|(_, entry)| entry).collect());
    for entry in drained {
        entry.listener.close();
        oauth::drop_pending(&entry.ticket);
    }
}

/// 换证结果 → `LoginPoll`（归一化在 `accounts`，失败翻成 `Failed`）。
fn done_poll(payload: &serde_json::Value) -> proto::LoginPoll {
    match Credential::from_payload(payload) {
        Ok(credential) => match accounts::account_of(&credential, "web-login", None).into_account() {
            Ok(account) => proto::LoginPoll::Done { account },
            Err(message) => proto::LoginPoll::Failed { message },
        },
        Err(message) => proto::LoginPoll::Failed { message },
    }
}

/// `LoginCallback` → 回调 query 串（带 `?` 时取问号后）。
fn callback_query(callback: &proto::LoginCallback) -> String {
    let url = callback.url.trim();
    if let Some((_, query)) = url.split_once('?') {
        return query.to_string();
    }
    if url.contains('=') {
        return url.to_string();
    }
    // 只给了 code：拼成最小的 query（`finish` 认得）
    if callback.code.trim().is_empty() {
        String::new()
    } else {
        format!("code={}", callback.code.trim())
    }
}

/// 查询串 → 键值对（**不做**解码：`finish` 里按需解）。
fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// `application/x-www-form-urlencoded` 的解码（`+` = 空格，`%XX` = 字节）。
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = &value[index + 1..index + 3];
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 日志里 ticket 取前 8 位就够定位。
fn short(ticket: &str) -> String {
    ticket.chars().take(8).collect()
}
