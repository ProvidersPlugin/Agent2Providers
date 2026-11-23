//! 固定任务服务结果的严格消费者；句柄只来自本次真实host create回包，不是远端UUID。
//! 不按任意JSON构造清理归属，不重试、回查任务列表或退回普通HTTP/WebSocket。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

use super::activity::{self, Activity};

const MAX_READS: usize = 4097;
const MAX_TEXT: usize = 256 * 1024;
const MAX_OUTPUT: usize = 8 * 1024 * 1024;
const MAX_INTEGER: i64 = (1_i64 << 53) - 1;

pub(super) struct Task {
    id: String,
    observed: bool,
    closed: bool,
    ended: bool,
    reads: usize,
    output_bytes: usize,
    activity_failed: bool,
}

pub(super) enum Item {
    Text { thought: bool, text: String },
    Activity(Activity),
    RoundEnded,
    Failed(Error),
}

struct Stage<'a> {
    status: &'a str,
    http: Option<u16>,
    code: Option<i64>,
}

struct State<'a> {
    id: Option<&'a str>,
    status: &'a str,
    submitted: bool,
    replay_forbidden: bool,
    creation: Stage<'a>,
}

/// 唯一Task构造点连同唯一create调用在此；不给上层自由id构造器或Clone。
pub(super) fn create(body: String) -> Result<Task> {
    let value = call("managed_task.create", json!({ "body": body }))?;
    let state = state(&value)?;
    if state.status != "created" {
        return Err(creation_error(&state));
    }
    if value["phase"] != "owned" || value["done"] != false {
        return Err(
            Error::http(502, "本次任务已进入收尾或失效，未恢复旧收据或重新创建")
                .with_code("monkeycode_task_no_longer_owned"),
        );
    }
    let id = state.id.ok_or_else(protocol)?.to_string();
    Ok(Task {
        id,
        observed: false,
        closed: false,
        ended: false,
        reads: 0,
        output_bytes: 0,
        activity_failed: false,
    })
}

impl Task {
    /// 仅作本地响应关联值；不能据此构造另一次操作或恢复远端任务。
    pub(super) fn correlation(&self) -> &str {
        &self.id
    }

    pub(super) fn observe(&mut self) -> Result<()> {
        if self.observed || self.closed {
            return Err(protocol());
        }
        self.observed = true;
        let value = call("managed_task.observe", json!({ "id": self.id }))?;
        shape(&value, &["id", "status", "done", "replay_forbidden"])?;
        self.matches(&value)?;
        if value["status"] != "observing"
            || value["done"] != false
            || value["replay_forbidden"] != true
        {
            return Err(protocol());
        }
        Ok(())
    }

    pub(super) fn read(&mut self) -> Result<Item> {
        if !self.observed || self.closed || self.ended || self.reads >= MAX_READS {
            return Err(protocol());
        }
        self.reads += 1;
        // host.read是阻塞等待本次观察结果，不sleep空转、不重新observe或重建句柄。
        let value = call("managed_task.read", json!({ "id": self.id }))?;
        self.matches(&value)?;
        match value["status"].as_str() {
            Some("text") => {
                shape(
                    &value,
                    &["id", "status", "kind", "text", "done", "replay_forbidden"],
                )?;
                if value["done"] != false || value["replay_forbidden"] != true {
                    return Err(protocol());
                }
                let thought = match value["kind"].as_str() {
                    Some("message") => false,
                    Some("thought") => true,
                    _ => return Err(protocol()),
                };
                let text = value["text"]
                    .as_str()
                    .filter(|text| !text.is_empty() && text.len() <= MAX_TEXT)
                    .ok_or_else(protocol)?;
                if text.len() > MAX_OUTPUT.saturating_sub(self.output_bytes) {
                    return Err(protocol());
                }
                self.output_bytes += text.len();
                Ok(Item::Text {
                    thought,
                    text: text.to_string(),
                })
            }
            Some("tool") => {
                let activity = Activity::from_read(&value)?;
                // 只累计不可清除事实，不按ID/title关联或重写此前工具活动。
                self.activity_failed |= activity.failed();
                let bytes = activity.read_bytes();
                if bytes > MAX_OUTPUT.saturating_sub(self.output_bytes) {
                    return Err(protocol());
                }
                self.output_bytes += bytes;
                Ok(Item::Activity(activity))
            }
            Some("end") => {
                shape(
                    &value,
                    &[
                        "id",
                        "status",
                        "outcome",
                        "interaction",
                        "message",
                        "done",
                        "replay_forbidden",
                        "task",
                    ],
                )?;
                if value["done"] != true || value["replay_forbidden"] != true {
                    return Err(protocol());
                }
                safe_message(&value["message"])?;
                let task = state(&value["task"])?;
                if task.id != Some(self.id.as_str())
                    || task.status != "created"
                    || !task.submitted
                    || !task.replay_forbidden
                {
                    return Err(protocol());
                }
                self.ended = true;
                match value["outcome"].as_str() {
                    Some("round_ended") if value["interaction"].is_null() => {
                        if self.activity_failed {
                            Ok(Item::Failed(activity::failed()))
                        } else {
                            Ok(Item::RoundEnded)
                        }
                    }
                    Some("needs_interaction")
                        if matches!(
                            value["interaction"].as_str(),
                            Some("permission" | "question")
                        ) =>
                    {
                        Ok(Item::Failed(
                            Error::http(
                                503,
                                "任务需要尚未迁移的真人交互，本轮未完成；未自动批准或答复",
                            )
                            .with_code("monkeycode_task_needs_interaction"),
                        ))
                    }
                    Some("failed") if value["interaction"].is_null() => Ok(Item::Failed(
                        Error::http(502, "任务观察失败或未确认正常结束，本轮未完成；不自动重放")
                            .with_code("monkeycode_task_observation_failed"),
                    )),
                    _ => Err(protocol()),
                }
            }
            _ => Err(protocol()),
        }
    }

    /// 至多一次本句柄结束意图；不把close_requested/acknowledged视为远端销毁。
    /// 无Drop网络动作：Trap或取消由宿主Owner保留原worker并承担本次收尾。
    pub(super) fn close(&mut self) -> Result<()> {
        if self.closed {
            return Err(protocol());
        }
        self.closed = true;
        let value = call("managed_task.close", json!({ "id": self.id }))?;
        shape(&value, &["id", "close_requested", "task"])?;
        self.matches(&value)?;
        if !value["close_requested"].is_boolean() {
            return Err(protocol());
        }
        let task = state(&value["task"])?;
        if task.id != Some(self.id.as_str())
            || task.status != "created"
            || !task.submitted
            || !task.replay_forbidden
        {
            return Err(protocol());
        }
        // close_requested=false可能是原worker已经收尾；既不猜销毁，也不追加停止或重试。
        Ok(())
    }

    fn matches(&self, value: &Value) -> Result<()> {
        if value["id"].as_str() != Some(self.id.as_str()) {
            return Err(protocol());
        }
        Ok(())
    }
}

fn call(method: &str, params: Value) -> Result<Value> {
    sdk::host(method, params).map_err(|error| {
        // 不回显宿主异常原文/URL/账号材料，也不把权限/取消错误改成可重试请求。
        let mut safe = Error::http(
            error
                .status
                .filter(|status| (400..600).contains(status))
                .unwrap_or(503),
            "受控任务服务未完成或未获本次授权；没有普通网络回落、换号或重试",
        )
        .with_code(
            error
                .code
                .unwrap_or_else(|| "monkeycode_task_service_unavailable".into()),
        );
        safe.upstream_code = error.upstream_code;
        safe.reset_at = error.reset_at;
        safe.credential_scope = error.credential_scope;
        safe
    })
}

fn state(value: &Value) -> Result<State<'_>> {
    shape(
        value,
        &[
            "id",
            "status",
            "phase",
            "submitted",
            "replay_forbidden",
            "retry_allowed",
            "creation",
            "stop",
            "done",
        ],
    )?;
    let id = match &value["id"] {
        Value::Null => None,
        Value::String(id) if handle(id) => Some(id.as_str()),
        _ => return Err(protocol()),
    };
    let status = value["status"].as_str().ok_or_else(protocol)?;
    let phase = value["phase"].as_str().ok_or_else(protocol)?;
    let submitted = value["submitted"].as_bool().ok_or_else(protocol)?;
    let replay_forbidden = value["replay_forbidden"].as_bool().ok_or_else(protocol)?;
    let done = value["done"].as_bool().ok_or_else(protocol)?;
    if value["retry_allowed"] != false {
        return Err(protocol());
    }
    match phase {
        "not_sent" | "preparing" if !submitted => {}
        "creating" | "owned" | "stopping" if submitted && !done => {}
        "finished" if done => {}
        _ => return Err(protocol()),
    }
    if submitted && (!replay_forbidden || id.is_none()) {
        return Err(protocol());
    }
    if done && submitted && phase != "finished" {
        return Err(protocol());
    }
    let creation = stage(&value["creation"], false)?;
    match status {
        "not_sent" if !submitted && creation.status == "not_sent" => {}
        "created" if submitted && creation.status == "created" && phase != "creating" => {}
        "rejected" if submitted && creation.status == "rejected" && phase == "finished" => {}
        // 可能已创建但没有交付可使用的真实收据；不把stage.created升级成Task。
        "unknown" if submitted && matches!(creation.status, "pending" | "unknown" | "created") => {}
        _ => return Err(protocol()),
    }
    if matches!(phase, "owned" | "stopping") && creation.status != "created" {
        return Err(protocol());
    }
    if !value["stop"].is_null() {
        if !submitted || creation.status != "created" {
            return Err(protocol());
        }
        stage(&value["stop"], true)?;
    } else if phase == "stopping" {
        return Err(protocol());
    }
    Ok(State {
        id,
        status,
        submitted,
        replay_forbidden,
        creation,
    })
}

fn stage(value: &Value, stop: bool) -> Result<Stage<'_>> {
    if stop {
        shape(
            value,
            &[
                "status",
                "http_status",
                "business_code",
                "message",
                "remote_destroyed",
            ],
        )?;
        if !value["remote_destroyed"].is_null() {
            return Err(protocol());
        }
    } else {
        shape(
            value,
            &["status", "http_status", "business_code", "message"],
        )?;
    }
    let status = value["status"].as_str().ok_or_else(protocol)?;
    let http = match &value["http_status"] {
        Value::Null => None,
        value => Some(
            value
                .as_u64()
                .filter(|code| (100..600).contains(code))
                .ok_or_else(protocol)? as u16,
        ),
    };
    let code = match &value["business_code"] {
        Value::Null => None,
        value => Some(
            value
                .as_i64()
                .filter(|code| (-MAX_INTEGER..=MAX_INTEGER).contains(code))
                .ok_or_else(protocol)?,
        ),
    };
    safe_message(&value["message"])?;
    match status {
        "not_sent" if !stop && http.is_none() && code.is_none() => {}
        "pending" if http.is_none() && code.is_none() => {}
        "created"
            if !stop
                && http.is_some_and(|status| (200..300).contains(&status))
                && code == Some(0) => {}
        "acknowledged"
            if stop
                && http.is_some_and(|status| (200..300).contains(&status))
                && code == Some(0) => {}
        "rejected"
            if http.is_some_and(|status| !(200..300).contains(&status))
                || code.is_some_and(|code| code != 0) => {}
        "unknown" => {}
        _ => return Err(protocol()),
    }
    Ok(Stage { status, http, code })
}

fn creation_error(state: &State<'_>) -> Error {
    let (status, code, message) = match (state.status, state.creation.code) {
        ("rejected", Some(10811)) => (
            409,
            "monkeycode_task_busy",
            "上游明确报告任务名额占用（10811）；未猜额度、枚举或停止其他任务",
        ),
        ("not_sent", _) => (
            503,
            "monkeycode_task_not_sent",
            "本次任务确定未提交或未获授权；本调用不再创建",
        ),
        ("rejected", _) => (
            state
                .creation
                .http
                .filter(|status| (400..600).contains(status))
                .unwrap_or(502),
            "monkeycode_task_rejected",
            "上游明确拒绝本次任务成功判定；不重新创建",
        ),
        _ => (
            502,
            "monkeycode_task_unknown",
            "本次任务创建结果未知；没有真实可用收据，禁止重放",
        ),
    };
    let mut error = Error::http(status, message).with_code(code);
    error.upstream_code = state.creation.code;
    error
}

fn handle(id: &str) -> bool {
    id.len() == 67
        && id.starts_with("mt-")
        && id[3..]
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        && id[3..].bytes().any(|byte| byte != b'0')
}

fn shape(value: &Value, fields: &[&str]) -> Result<()> {
    if !value.as_object().is_some_and(|object| {
        object.len() == fields.len() && object.keys().all(|key| fields.contains(&key.as_str()))
    }) {
        return Err(protocol());
    }
    Ok(())
}

fn safe_message(value: &Value) -> Result<()> {
    if !value.as_str().is_some_and(|text| {
        !text.is_empty() && text.len() <= 4096 && !text.chars().any(char::is_control)
    }) {
        return Err(protocol());
    }
    Ok(())
}

fn protocol() -> Error {
    Error::http(
        502,
        "受控任务结果缺字段、互相矛盾或超限；未猜成功、收据或用量，不重放",
    )
    .with_code("monkeycode_task_protocol_error")
}
