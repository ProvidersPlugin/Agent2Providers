//! AStudio 的本机登录页流程（手机号验证码 / 账号密码 + 极验）。
//!
//! ── 为什么走本机网页而不是系统浏览器 ────────────────────────
//! 登录要完成极验滑块，页面由插件自己起在 `127.0.0.1` 上；内嵌窗口里
//! 完成验证后结果直接回到插件，不依赖系统浏览器的回环跳转。
//!
//! ── 为什么是轮询型 ──────────────────────────────────────────
//! 网关的登录任务框架要求一个可轮询的 flow；页面完成登录时把结果放进
//! 进程内的表，`login.poll` 下一次调用就返回 `Done`。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use agent2api_provider_protocol as proto;

use super::accounts;

/// 一轮登录的进程内状态：`result` 为空 = 还在等（网页登录），
/// `abort` 用于取消时关掉本机登录页的监听。
#[derive(Default)]
struct Entry {
    result: Option<Result<Value, String>>,
    abort: Option<tokio::task::AbortHandle>,
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

/// 把一次登录的结果放进表里（网页登录页的 API 调它）。
pub fn store_result(flow: &str, result: Result<Value, String>) {
    with_flows(|table| {
        if let Some(entry) = table.get_mut(flow) {
            entry.result = Some(result);
        }
    });
}

/// 登记一轮网页登录（结果还没有）。
pub fn open_flow(flow: &str) {
    with_flows(|table| {
        table.insert(
            flow.to_string(),
            Entry {
                result: None,
                abort: None,
            },
        );
    });
}

/// 登记网页登录页的任务句柄（取消时关端口）。
pub fn attach_abort(flow: &str, handle: tokio::task::AbortHandle) {
    with_flows(|table| {
        if let Some(entry) = table.get_mut(flow) {
            entry.abort = Some(handle);
        }
    });
}

/// 开启一个网页登录流程（手机号验证码 / 账号密码）。
pub async fn start_web(
    proxy: Option<proto::WireProxy>,
    gateway_proxy: String,
) -> Result<proto::LoginStart, String> {
    super::login_page::start(proxy, gateway_proxy).await
}

/// 轮询一次：网页登录要等页面上的操作。
pub fn poll(flow: &str) -> proto::LoginPoll {
    let (result, exists) = with_flows(|table| {
        let exists = table.contains_key(flow);
        (table.get(flow).and_then(|entry| entry.result.clone()), exists)
    });
    if !exists {
        return proto::LoginPoll::Failed {
            message: "这次登录已失效，请重新发起".into(),
        };
    }
    match result {
        Some(Ok(secret)) => {
            with_flows(|table| {
                if let Some(entry) = table.remove(flow) {
                    if let Some(abort) = entry.abort {
                        abort.abort();
                    }
                }
            });
            proto::LoginPoll::Done {
                account: proto::NormalizedAccount {
                    identity: accounts::identity(&secret),
                    public: accounts::public(&secret),
                    secret,
                    desktop: false,
                },
            }
        }
        Some(Err(message)) => {
            with_flows(|table| {
                table.remove(flow);
            });
            proto::LoginPoll::Failed { message }
        }
        None => proto::LoginPoll::Pending { hint: None },
    }
}

pub fn cancel(flow: &str) {
    with_flows(|table| {
        if let Some(entry) = table.remove(flow) {
            if let Some(abort) = entry.abort {
                abort.abort();
            }
        }
    });
}

/// 界面上的两种登录方式（网关 `auth.methods` 的 camelCase 契约）。
///
/// `embeddedOnly` 是插件对界面申报的「这条流程只在应用内嵌窗口打开」：
/// AStudio 的登录页跑在本机回环地址上，系统浏览器打开没有必要，也会让
/// 用户多做一个选择。方法序号保留原来的 1 / 2（粘贴方式不走登录流程），
/// 只是把已移除的桌面端登录态腾出的 0 留空。
pub fn login_methods() -> Value {
    json!({
        "methods": [
            {
                "methodIndex": 1,
                "type": "oauth",
                "label": "手机号 / 账号密码登录",
                "prompts": [],
                "localOnly": false,
                "manualCallback": false,
                "callbackInput": "url",
                "embeddedOnly": true
            },
            {
                "methodIndex": 2,
                "type": "api",
                "label": "粘贴 modelBearerToken",
                "instructions": "粘贴 modelBearerToken；填了网页会话才能查余额与签到。",
                "prompts": [
                    {
                        "key": "modelBearerToken",
                        "type": "text",
                        "label": "modelBearerToken",
                        "placeholder": "调模型用的凭据",
                        "secret": true,
                        "optional": false
                    },
                    {
                        "key": "uid",
                        "type": "text",
                        "label": "账号 uid",
                        "placeholder": "可选，用于显示",
                        "optional": true
                    },
                    {
                        "key": "sessionToken",
                        "type": "text",
                        "label": "网页会话 token",
                        "placeholder": "astron-session.json 的 token；填了才有余额与签到",
                        "secret": true,
                        "optional": true
                    },
                    {
                        "key": "accountId",
                        "type": "text",
                        "label": "账号面 accountId",
                        "placeholder": "与网页会话 token 一起填",
                        "optional": true
                    },
                    {
                        "key": "ssoSessionId",
                        "type": "text",
                        "label": "ssoSessionId",
                        "placeholder": "可选，Cookie 会话的补充材料",
                        "secret": true,
                        "optional": true
                    }
                ],
                "localOnly": false,
                "manualCallback": false,
                "callbackInput": "url"
            }
        ]
    })
}
