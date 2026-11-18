//! 两地纯文本任务规划；不把插件JSON、展示目录或UA预检当作宿主执行许可。
//! 所有无法表达的入站材料在任务create之前拒绝，不静默丢工具、附件或后续system。

use std::io;

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

use crate::{catalog, endpoints, util};
use sdk::tool_bridge::ToolBridge;

const MAX_BODY: usize = 256 * 1024;
const MAX_MESSAGES: usize = 512;
const MAX_PARTS: usize = 512;

pub(super) struct Prepared {
    pub(super) body: String,
    pub(super) model: String,
    pub(super) created: i64,
    pub(super) bridge: ToolBridge,
}

pub(super) fn prepare(params: &Value) -> Result<Prepared> {
    let requested_account = params["account"]
        .as_str()
        .filter(|id| text_id(id, 512))
        .ok_or_else(|| invalid("当前对话没有明确账号，未准备任务"))?;
    if params["provider"] != endpoints::ID || params["stream"] != true {
        return Err(invalid(
            "当前输入不是对应地区插件的流式对话入口，未准备任务",
        ));
    }
    let body = params["body"]
        .as_object()
        .ok_or_else(|| invalid("国内文本对话请求必须是JSON对象"))?;
    if let Some(format) = body.get("response_format").filter(|value| !value.is_null()) {
        if format["type"] != "text" {
            return Err(unmigrated("上游任务接口不提供强制结构化输出"));
        }
    }
    for key in [
        "temperature",
        "top_p",
        "stop",
        "logprobs",
        "seed",
        "frequency_penalty",
        "presence_penalty",
        "max_tokens",
        "max_completion_tokens",
    ] {
        if body.get(key).is_some_and(|value| !value.is_null()) {
            return Err(unmigrated("上游任务接口不提供采样及停止参数"));
        }
    }
    if body.get("n").is_some_and(|value| value.as_u64() != Some(1)) {
        return Err(invalid("任务接口一次只能生成一个回答"));
    }
    if body.get("stream").is_some_and(|value| !value.is_boolean()) {
        return Err(invalid("stream必须是布尔值，未创建任务"));
    }
    if let Some(options) = body.get("stream_options") {
        let options = options
            .as_object()
            .ok_or_else(|| invalid("stream_options必须是对象"))?;
        if options.keys().any(|key| key != "include_usage") {
            return Err(unmigrated("额外流式选项尚未迁移，未创建任务"));
        }
        if options
            .get("include_usage")
            .is_some_and(|value| !value.is_boolean())
        {
            return Err(invalid("include_usage必须是布尔值"));
        }
        // 该选项不授权杜撰usage；宿主没有真实计数时仍不输出用量帧。
    }
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| text_id(model, 481))
        .ok_or_else(|| invalid("请求没有明确的模型显示ID，未创建任务"))?;
    let requested = params.get("wire_model").map_or(Ok(model), |wire| {
        wire.as_str()
            .filter(|wire| text_id(wire, 481))
            .ok_or_else(|| invalid("宿主实际发送模型无效，未使用展示模型替代"))
    })?;
    let mut budget = MAX_BODY;
    let history = body["messages"]
        .as_array()
        .filter(|rows| !rows.is_empty() && rows.len() <= MAX_MESSAGES)
        .ok_or_else(|| invalid("messages 必须是非空且大小受限的数组"))?;
    let mut has_dialogue = false;
    for row in history {
        let fields = row
            .as_object()
            .ok_or_else(|| invalid("对话消息必须是对象"))?;
        let role = row["role"]
            .as_str()
            .ok_or_else(|| invalid("消息角色必须是文本"))?;
        if !matches!(role, "system" | "developer" | "user" | "assistant" | "tool") {
            return Err(invalid("消息角色无效"));
        }
        let has_tool_calls = row["tool_calls"]
            .as_array()
            .is_some_and(|calls| !calls.is_empty());
        let text = match fields.get("content") {
            Some(Value::Null) | None if role == "assistant" && has_tool_calls => String::new(),
            Some(value) => content(value, &mut budget)?,
            None => return Err(invalid("消息缺少文本内容")),
        };
        has_dialogue |= matches!(role, "user" | "assistant" | "tool")
            && (!text.trim().is_empty() || has_tool_calls);
    }
    if !has_dialogue {
        return Err(invalid("对话没有可发送的文本或工具调用"));
    }
    let bridge = ToolBridge::new(&params["body"])?;
    let (content, system) = if bridge.active()
        || body["messages"].as_array().is_some_and(|rows| {
            rows.iter().any(|row| {
                row["role"] == "tool"
                    || row["role"] == "developer"
                    || row.get("tool_calls").is_some()
                    || row.as_object().is_some_and(|fields| {
                        fields
                            .keys()
                            .any(|key| !matches!(key.as_str(), "role" | "content"))
                    })
                    || rows.iter().skip(1).any(|row| row["role"] == "system")
            })
        }) {
        let text = format!("{}{}", bridge.instruction(), body["messages"]);
        if text.len() > MAX_BODY {
            return Err(invalid("工具声明与对话历史超过任务输入上限"));
        }
        (text, None)
    } else {
        prompt(body.get("messages"))?
    };
    // 只确认宿主有真实请求输出上下文；它不是ConversationCall/批准凭证。
    // 缺输出上下文或能力时预先拒绝，真实call与RowSource仍由managed_task服务裁决。
    sdk::request::context()
        .map_err(|_| invalid("主程序没有授予本次对话输出上下文，未取得任务来源或创建任务"))?;
    let current = Frozen(sdk::account()?);
    if current.0["id"].as_str() != Some(requested_account)
        || current.0["provider"] != endpoints::ID
        || current.0["public"]["enabled"] != true
        || !current.0["identity"]
            .as_str()
            .is_some_and(|identity| text_id(identity, 1024))
    {
        return Err(invalid("本次账号或身份来源不一致，未创建任务"));
    }
    let access = task_session(&current.0["secret"])?;
    let ua = current.0["public"]["userAgent"]
        .as_str()
        .ok_or_else(|| invalid("原账号没有明确User-Agent，未猜测默认值"))?;
    if ua.trim().is_empty()
        || ua.len() > 1024
        || !ua.bytes().all(|byte| matches!(byte, 0x20..=0x7e))
    {
        return Err(invalid("原账号User-Agent无效，未创建任务"));
    }
    let image = if ["imageId", "image_id"].into_iter().any(|key| {
        current.0["secret"]
            .get(key)
            .is_some_and(|value| !value.is_null() && value.as_str() != Some(""))
    }) {
        image_id(&current.0["secret"])?
    } else {
        sdk::monkeycode_checkin::default_image(endpoints::ID, access)?
    };
    // UUID映射按宿主实际wire_model精确查询；原body.model只用于下游展示，不进行默认回落。
    let target = catalog::target(&json!({
        "account": requested_account, "payload": { "model": requested },
    }))?;
    if !target["id"]
        .as_str()
        .is_some_and(|id| id.eq_ignore_ascii_case(requested))
        || target["displayOnly"] != true
    {
        return Err(invalid("本账号没有准确的展示模型映射，未回落默认模型"));
    }
    let model_id = target["model_id"]
        .as_str()
        .filter(|id| uuid(id))
        .ok_or_else(|| invalid("本账号模型没有真实非零UUID，未创建任务"))?;
    // target仅展示真实配置UUID，不宣称完整账号权限或宿主来源已获批准。
    let mut fixed = json!({
        "content": content, "model_id": model_id.to_ascii_lowercase(), "image_id": image,
        "host_id": "public_host", "cli_name": "opencode",
        "resource": { "core": 1, "memory": 1073741824u64, "life": 3600 },
        "repo": { "repo_url": "", "branch": "master", "repo_filename": "", "zip_url": "" },
    });
    if let Some(system) = system {
        fixed["system_prompt"] = json!(system);
    }
    let body = encode(&fixed)?;
    let created = util::now()? / 1000;
    current.unchanged()?;
    Ok(Prepared {
        body,
        model: model.to_string(),
        created,
        bridge,
    })
}

struct Frozen(Value);
impl Frozen {
    fn unchanged(&self) -> Result<()> {
        let next = sdk::account()?;
        if next["id"] != self.0["id"]
            || next["identity"] != self.0["identity"]
            || next["provider"] != self.0["provider"]
            || next["secret"] != self.0["secret"]
            || next["public"]["enabled"] == false
            || next["public"]["userAgent"] != self.0["public"]["userAgent"]
        {
            return Err(invalid("当前账号在准备任务时发生变化，请重试"));
        }
        Ok(())
    }
}

fn prompt(messages: Option<&Value>) -> Result<(String, Option<String>)> {
    let messages = messages
        .and_then(Value::as_array)
        .filter(|messages| !messages.is_empty() && messages.len() <= MAX_MESSAGES)
        .ok_or_else(|| invalid("messages必须是非空且大小受限的数组"))?;
    let mut prompt = String::new();
    let mut system = None;
    let mut budget = MAX_BODY;
    for (index, message) in messages.iter().enumerate() {
        let fields = message
            .as_object()
            .ok_or_else(|| invalid("对话消息必须是对象"))?;
        if fields.len() != 2 || !fields.contains_key("role") || !fields.contains_key("content") {
            return Err(unmigrated(
                "工具调用、工具结果、命名消息及额外消息字段尚未迁移，未创建任务",
            ));
        }
        let role = fields["role"]
            .as_str()
            .ok_or_else(|| invalid("消息role必须是文本"))?;
        if !matches!(role, "system" | "user" | "assistant") {
            return Err(unmigrated(
                "非文本角色、工具结果及developer消息尚未迁移，未创建任务",
            ));
        }
        if role == "system" && (index != 0 || system.is_some()) {
            return Err(unmigrated(
                "后续system消息无法无损表达为单个system_prompt，未丢弃或重排，未创建任务",
            ));
        }
        let text = content(&fields["content"], &mut budget)?;
        if text.trim().is_empty() {
            return Err(invalid("消息没有明确文本内容，未静默跳过该消息"));
        }
        if role == "system" {
            system = Some(text);
            continue;
        }
        if !prompt.is_empty() {
            append(&mut prompt, "\n\n")?;
        }
        append(
            &mut prompt,
            if role == "user" {
                "[User]\n"
            } else {
                "[Assistant]\n"
            },
        )?;
        append(&mut prompt, &text)?;
    }
    if prompt.trim().is_empty() {
        return Err(invalid("只有system而没有可发送的对话文本，未创建任务"));
    }
    Ok((prompt, system))
}

fn content(value: &Value, budget: &mut usize) -> Result<String> {
    if let Some(text) = value.as_str() {
        take_budget(text, budget)?;
        return Ok(text.to_string());
    }
    let parts = value
        .as_array()
        .filter(|parts| !parts.is_empty() && parts.len() <= MAX_PARTS)
        .ok_or_else(|| unmigrated("非文本消息内容尚未迁移，未创建任务"))?;
    let mut text = String::new();
    for part in parts {
        let fields = part
            .as_object()
            .ok_or_else(|| invalid("文本消息部件必须是对象"))?;
        if fields.len() != 2
            || !matches!(
                fields.get("type").and_then(Value::as_str),
                Some("text" | "input_text" | "output_text")
            )
            || !fields.contains_key("text")
        {
            return Err(unmigrated(
                "图片、文件、资源及非文本部件尚未迁移，未创建任务",
            ));
        }
        let part = fields["text"]
            .as_str()
            .ok_or_else(|| invalid("文本部件的text必须是文本"))?;
        take_budget(part, budget)?;
        append(&mut text, part)?;
    }
    Ok(text)
}

fn task_session(secret: &Value) -> Result<&str> {
    let mut selected = None;
    for key in ["accessToken", "session", "token"] {
        let Some(raw) = secret.get(key).filter(|value| !value.is_null()) else {
            continue;
        };
        let candidate = raw
            .as_str()
            .ok_or_else(|| invalid("原账号Session字段格式无效"))?
            .trim();
        if candidate.is_empty() {
            continue;
        }
        if candidate.len() > 8192
            || !candidate.bytes().all(
                |byte| matches!(byte, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e),
            )
            || selected.is_some_and(|previous| previous != candidate)
        {
            return Err(invalid("原账号Session投影无效或矛盾，未用其他凭据替代"));
        }
        selected = Some(candidate);
    }
    selected.ok_or_else(|| invalid("原账号没有任务所需的Session，未补造凭据"))
}

fn image_id(secret: &Value) -> Result<String> {
    let mut selected: Option<String> = None;
    for key in ["imageId", "image_id"] {
        let Some(value) = secret
            .get(key)
            .filter(|value| !value.is_null() && value.as_str() != Some(""))
        else {
            continue;
        };
        let id = value
            .as_str()
            .filter(|id| uuid(id))
            .ok_or_else(|| invalid("执行环境镜像编号不是有效非零UUID，未创建任务"))?
            .to_ascii_lowercase();
        if selected.as_ref().is_some_and(|previous| previous != &id) {
            return Err(invalid("执行环境镜像编号别名冲突，未任选一项"));
        }
        selected = Some(id);
    }
    selected.ok_or_else(|| invalid("账号缺少执行环境镜像UUID，未枚举历史任务或猜测镜像"))
}

pub(super) fn uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
        && id
            .bytes()
            .any(|byte| matches!(byte, b'1'..=b'9' | b'a'..=b'f' | b'A'..=b'F'))
}

fn text_id(text: &str, limit: usize) -> bool {
    !text.is_empty()
        && text.len() <= limit
        && text.trim() == text
        && !text.chars().any(char::is_control)
}

fn take_budget(text: &str, budget: &mut usize) -> Result<()> {
    if text.len() > *budget {
        return Err(invalid("入站文本超过固定任务正文预算，未创建任务"));
    }
    *budget -= text.len();
    Ok(())
}

fn append(target: &mut String, text: &str) -> Result<()> {
    if text.len() > MAX_BODY.saturating_sub(target.len()) {
        return Err(invalid("任务文本超过256KiB预算，未创建任务"));
    }
    target.push_str(text);
    Ok(())
}

fn encode(value: &Value) -> Result<String> {
    struct Bounded(Vec<u8>);
    impl io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > MAX_BODY.saturating_sub(self.0.len()) {
                return Err(io::Error::other("task body limit"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut output = Bounded(Vec::new());
    serde_json::to_writer(&mut output, value)
        .map_err(|_| invalid("固定任务JSON超过256KiB或无法编码，未创建任务"))?;
    String::from_utf8(output.0).map_err(|_| invalid("固定任务JSON编码无效，未创建任务"))
}

pub(super) fn invalid(message: &'static str) -> Error {
    Error::http(400, message).with_code("monkeycode_conversation_bad_input")
}

fn unmigrated(message: &'static str) -> Error {
    Error::http(400, message).with_code("unsupported_parameter")
}
