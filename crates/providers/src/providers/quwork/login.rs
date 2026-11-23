use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use agent2api_provider_protocol as proto;
use serde_json::{json, Value};

use super::{accounts, desktop};

fn flows() -> &'static Mutex<HashMap<String, (i64, proto::NormalizedAccount)>> {
    static FLOWS: OnceLock<Mutex<HashMap<String, (i64, proto::NormalizedAccount)>>> =
        OnceLock::new();
    FLOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn methods() -> Value {
    json!({ "methods": [
        {
            "methodIndex": 0, "type": "oauth", "label": "导入 QuWork 本机登录态",
            "instructions": "读取运行网关的电脑上已登录的 QuWork，只保存本机引用，不修改官方客户端文件；切换账号后需重新导入。",
            "prompts": [], "localOnly": true, "manualCallback": false, "callbackInput": "url",
        },
        {
            "methodIndex": 1, "type": "api", "label": "QuWork 凭证",
            "instructions": "粘贴完整登录凭证 JSON，或只填模型凭证 llmToken。仅模型凭证可对话；积分、续期和奖励需要登录凭证 accessToken。",
            "prompts": [
                { "key": "credentials", "type": "text", "label": "凭证 JSON 或模型凭证", "placeholder": "{\"accessToken\":\"…\",\"llmToken\":\"…\"}", "secret": true, "optional": false },
                { "key": "accessToken", "type": "text", "label": "登录凭证 accessToken", "placeholder": "完整 JSON 已包含时无需填写", "secret": true, "optional": true },
                { "key": "deviceIdentifier", "type": "text", "label": "QuWork 原设备标识", "placeholder": "可选，仅远程兑换码激活需要；从官方客户端读取", "secret": true, "optional": true },
            ],
            "localOnly": false, "manualCallback": false, "callbackInput": "url",
        },
    ] })
}

pub async fn start(method: &str, payload: &Value) -> Result<proto::LoginStart, String> {
    if method != "browser"
        || payload
            .get("methodIndex")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            != 0
    {
        return Err("QuWork 请使用本机导入或粘贴凭证".into());
    }
    let secret = desktop::reference()
        .await
        .map_err(|error| error.into_control_message())?;
    let account = accounts::normalized(secret, None, true);
    let mut bytes = [0_u8; 24];
    getrandom::getrandom(&mut bytes).map_err(|_| "无法生成 QuWork 导入标识".to_string())?;
    let flow = format!(
        "quwork-{}",
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let now = crate::http::now_ms();
    let mut table = flows()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    table.retain(|_, (created, _)| now.saturating_sub(*created) < 120_000);
    if table.len() >= 32 {
        return Err("QuWork 导入请求过多，请稍后重试".into());
    }
    table.insert(flow.clone(), (now, account));
    Ok(proto::LoginStart {
        flow,
        url: String::new(),
        instructions: "已读取 QuWork 本机登录态，正在添加账号。".into(),
        poll_interval_ms: 1000,
        external_browser: false,
        allowed_hosts: Vec::new(),
    })
}

pub fn poll(flow: &str) -> proto::LoginPoll {
    let found = flows()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(flow);
    match found {
        Some((at, account)) if crate::http::now_ms().saturating_sub(at) < 120_000 => {
            proto::LoginPoll::Done { account }
        }
        _ => proto::LoginPoll::Failed {
            message: "QuWork 导入已失效，请重新发起".into(),
        },
    }
}

pub fn cancel(flow: &str) {
    flows()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(flow);
}
