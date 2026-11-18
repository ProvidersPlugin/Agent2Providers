//! 国内成长域，有限同步调用经受控SDK；任务请求与普通账号出口绑定。
//! 原有上报指纹/版本是协议兼容常量，不是读取或宣称宿主硬件信息。
pub mod daily;
pub mod events;
mod expert;
pub mod school;
pub mod spec;
pub mod status;
pub mod streak;
mod task;
pub mod travel;

use crate::{
    account, endpoints, errors,
    ledger::{self, Job},
    network,
    session::{self, Session},
    util, web_session,
};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub const CHAT_BASE: &str = endpoints::CHAT;
pub const BILL_BASE: &str = endpoints::BILL;
pub const WEB_BASE: &str = endpoints::ORIGIN;
pub const GROWTH_WRITE_GAP_MS: u64 = 1200;
pub const DESKTOP_UA: &str = "WorkBuddy/5.5.6 WorkBuddy/5.5.6 CLI/2.137.1";
pub const CLI_UA: &str = "CLI/2.63.2 CodeBuddy/2.63.2";
pub const WEB_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36";
pub const MP_UA: &str = "Mozilla/5.0 (Linux; Android 14; MicroMessenger/8.0.49 WeChat/0.8.0 MiniProgramEnv/android; wkbrowser xweb)";

pub struct GrowthAccount {
    pub uid: String,
    pub nick: String,
    pub token: String,
    pub domain: String,
    pub session: Session,
    pub job: Option<Job>,
}
impl GrowthAccount {
    pub fn new(current: Session, action: Option<&str>, until: i64) -> Result<Self> {
        if !account::personal(&current.secret) {
            return Err(Error::http(400, "企业账号没有个人成长任务"));
        }
        let job = action
            .map(|label| Job::begin(&current, label, until))
            .transpose()?;
        Ok(Self {
            uid: util::text(&current.secret, &["uid"]),
            nick: util::text(&current.secret, &["nickname", "name"]),
            token: current.token().to_string(),
            domain: util::text(&current.secret, &["domain"]),
            session: current,
            job,
        })
    }
    pub fn derive_id(&self, salt: &str) -> String {
        use md5::{Digest, Md5};
        util::hex(&Md5::digest(format!("{salt}:{}", self.uid).as_bytes()))
    }
    pub fn check(&self) -> std::result::Result<(), String> {
        if let Some(job) = &self.job {
            job.check().map_err(|e| e.message)
        } else {
            session::check(&self.session).map_err(|e| e.message)
        }
    }
    pub fn pause(&self, millis: u64) -> std::result::Result<(), String> {
        if let Some(job) = &self.job {
            job.pause(millis).map_err(|e| e.message)
        } else {
            self.check()?;
            sdk::sleep(millis).map_err(|e| e.message)?;
            self.check()
        }
    }
}
pub struct GrowthCall {
    pub status: u16,
    pub payload: Option<Value>,
}
impl GrowthCall {
    pub fn code(&self) -> Option<i64> {
        self.payload.as_ref().and_then(errors::code)
    }
    pub fn msg(&self) -> String {
        self.payload
            .as_ref()
            .map(errors::message)
            .unwrap_or("")
            .to_string()
    }
    pub fn ok(&self) -> bool {
        self.payload
            .as_ref()
            .is_some_and(|value| !errors::failed(self.status, value))
    }
    pub fn data(&self) -> &Value {
        self.payload
            .as_ref()
            .and_then(|v| v.get("data"))
            .unwrap_or(&Value::Null)
    }
}
pub fn growth_request(
    account: &GrowthAccount,
    method: &str,
    url: &str,
    body: Option<&Value>,
    extra: &[(String, String)],
) -> std::result::Result<GrowthCall, String> {
    account.check()?;
    let url = url::Url::parse(url).map_err(|_| "成长任务地址无效".to_string())?;
    let origin = url.origin().ascii_serialization();
    if ![CHAT_BASE, BILL_BASE, WEB_BASE].contains(&origin.as_str()) {
        return Err("成长任务不允许向未声明域发送凭据".into());
    }
    let path = format!(
        "{}{}",
        url.path(),
        url.query().map(|q| format!("?{q}")).unwrap_or_default()
    );
    if method != "GET" {
        throttle(account)?;
    }
    let mut extra_value = json!({});
    for (key, value) in extra {
        extra_value[key] = json!(value);
    }
    let reply = if origin == WEB_BASE {
        web_session::request(&account.session, method, &path, body, extra_value)
    } else {
        let mut headers = json!({ "Authorization": format!("Bearer {}", account.token), "Accept": "application/json",
            "Content-Type": "application/json", "User-Agent": CLI_UA, "Origin": BILL_BASE, "Referer": format!("{BILL_BASE}/"), "X-User-Id": account.uid });
        if !account.domain.is_empty() { headers["X-Domain"] = json!(account.domain); }
        crate::headers::merge(&mut headers, extra_value);
        network::raw(method, &origin, &path, headers, body)
    }.map_err(|e| errors::clean(&e.message, &account.session.secrets()))?;
    account.check()?;
    let mut payload = reply.value;
    if errors::failed(reply.status, &payload) {
        let message = errors::clean(errors::message(&payload), &account.session.secrets());
        payload = json!({ "code": errors::code(&payload), "message": message, "msg": message, "error": { "message": message } });
    }
    Ok(GrowthCall {
        status: reply.status,
        payload: Some(payload),
    })
}
fn throttle(account: &GrowthAccount) -> std::result::Result<(), String> {
    let key = "growth-write-gap";
    let before = ledger::load(key).map_err(|e| e.message)?;
    let binding = account::fingerprint(&account.session.secret);
    let at = if before.value["binding"] == binding {
        before.value["at"].as_i64().unwrap_or(0)
    } else {
        0
    };
    let wait = at
        .saturating_add(GROWTH_WRITE_GAP_MS as i64)
        .saturating_sub(sdk::now());
    if wait > 0 {
        account.pause(wait.min(60_000) as u64)?;
    }
    ledger::store(
        key,
        &before,
        json!({ "binding": binding, "at": sdk::now() }),
    )
    .map_err(|e| e.message)
}
pub fn chat_call(
    account: &GrowthAccount,
    method: &str,
    path: &str,
    body: Option<&Value>,
    headers: &[(String, String)],
) -> std::result::Result<GrowthCall, String> {
    growth_request(
        account,
        method,
        &format!("{CHAT_BASE}{path}"),
        body,
        headers,
    )
}
pub fn bill_call(
    account: &GrowthAccount,
    method: &str,
    path: &str,
    body: Option<&Value>,
    headers: &[(String, String)],
) -> std::result::Result<GrowthCall, String> {
    growth_request(
        account,
        method,
        &format!("{BILL_BASE}{path}"),
        body,
        headers,
    )
}
pub fn web_call(
    account: &GrowthAccount,
    method: &str,
    path: &str,
    body: Option<&Value>,
    headers: &[(String, String)],
) -> std::result::Result<GrowthCall, String> {
    growth_request(account, method, &format!("{WEB_BASE}{path}"), body, headers)
}
pub fn list_tasks(account: &GrowthAccount, mp: bool) -> std::result::Result<Vec<Value>, String> {
    let reply = chat_call(
        account,
        "GET",
        "/v2/activity/growth/tasks",
        None,
        &events::mp_platform_header(mp),
    )?;
    if !reply.ok() {
        return Err(format!(
            "成长任务列表失败（HTTP {}）：{}",
            reply.status,
            reply.msg()
        ));
    }
    reply.data()["tasks"]
        .as_array()
        .cloned()
        .ok_or_else(|| "成长任务列表缺少tasks".into())
}
pub fn find_task(tasks: &[Value], code: &str) -> Option<Value> {
    tasks.iter().find(|v| v["task_code"] == code).cloned()
}
pub fn energy_balance(account: &GrowthAccount) -> Option<i64> {
    let reply = chat_call(account, "GET", "/v2/activity/growth/energy", None, &[]).ok()?;
    reply
        .ok()
        .then(|| util::integer(&reply.data()["balance"]))
        .flatten()
}
pub fn growth_streak(account: &GrowthAccount) -> Option<i64> {
    let reply = chat_call(account, "GET", "/activity/growth/streak", None, &[]).ok()?;
    reply
        .ok()
        .then(|| util::integer(&reply.data()["streak"]["days"]))
        .flatten()
}
pub fn run_task(account: &GrowthAccount, payload: &Value) -> Result<Value> {
    task::run(account, payload).map_err(|message| Error::http(502, message))
}
