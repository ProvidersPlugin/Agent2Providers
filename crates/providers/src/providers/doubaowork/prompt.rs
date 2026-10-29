use std::collections::BTreeSet;

use serde_json::{json, Value};

use super::catalog::Model;
use crate::errors::GatewayError;

pub struct ToolBridge {
    marker: String,
    names: BTreeSet<String>,
    forced: bool,
    parallel: bool,
    declarations: Value,
}

fn unsupported(message: &str) -> GatewayError {
    GatewayError::with_status(400, message).with_code("unsupported_parameter")
}

impl ToolBridge {
    pub fn new(body: &Value) -> Result<Self, GatewayError> {
        let rows = body
            .get("tools")
            .filter(|value| !value.is_null())
            .map(|value| {
                value
                    .as_array()
                    .ok_or_else(|| unsupported("tools 必须是数组"))
            })
            .transpose()?
            .cloned()
            .unwrap_or_default();
        let mut names = BTreeSet::new();
        for row in &rows {
            let name = row["function"]["name"]
                .as_str()
                .filter(|name| !name.is_empty())
                .ok_or_else(|| unsupported("工具缺少 function.name"))?;
            if row["type"] != "function" || !names.insert(name.to_string()) {
                return Err(unsupported("豆包工作只接受名称唯一的 function 工具"));
            }
        }
        let choice = body.get("tool_choice").filter(|value| !value.is_null());
        let forced = match choice {
            None => false,
            Some(Value::String(choice)) if choice == "auto" => false,
            Some(Value::String(choice)) if choice == "none" => {
                names.clear();
                false
            }
            Some(Value::String(choice)) if choice == "required" && !names.is_empty() => true,
            Some(choice) if choice.is_object() => {
                let name = choice["function"]["name"].as_str().unwrap_or("");
                if choice["type"] != "function" || !names.contains(name) {
                    return Err(unsupported("tool_choice 必须选择已声明的工具"));
                }
                names.retain(|item| item == name);
                true
            }
            _ => return Err(unsupported("无效的 tool_choice")),
        };
        Ok(Self {
            marker: crate::ids::random_id("agent2api_tools"),
            names,
            forced,
            parallel: body["parallel_tool_calls"] != false,
            declarations: json!(rows),
        })
    }

    pub fn active(&self) -> bool {
        !self.names.is_empty()
    }

    fn instruction(&self) -> String {
        if self.names.is_empty() {
            return String::new();
        }
        format!("The following JSON conversation contains the complete client history, including tool call IDs and tool results. Follow its system/developer instructions. Client-side tools are available; do not run substitute desktop tools. To request tools, return ONLY a JSON object with key {} and an array of objects {{\"name\":\"declared name\",\"arguments\":{{...}}}}. Allowed names: {}. Tool definitions: {}. A tool result is provided in the next request. Otherwise return your answer as text. Tool use is {}. Parallel tool use is {}.\n", json!(self.marker), json!(self.names), self.declarations, if self.forced { "required" } else { "optional" }, self.parallel)
    }

    pub fn message(&self, text: &str) -> Result<Value, GatewayError> {
        let parsed = serde_json::from_str::<Value>(text.trim()).ok();
        let calls = parsed.as_ref().and_then(|value| value.get(&self.marker));
        if calls.is_none() && text.contains(&self.marker) {
            return Err(GatewayError::with_status(
                502,
                "豆包工作返回了不完整的工具调用",
            ));
        }
        if let Some(calls) = calls {
            let calls = calls
                .as_array()
                .filter(|calls| !calls.is_empty())
                .ok_or_else(|| GatewayError::with_status(502, "豆包工作返回的工具调用格式无效"))?;
            if !self.parallel && calls.len() > 1 {
                return Err(GatewayError::with_status(
                    502,
                    "豆包工作未遵守 parallel_tool_calls=false",
                ));
            }
            let mut normalized = Vec::new();
            for call in calls {
                let name = call["name"].as_str().unwrap_or("");
                if !self.names.contains(name) || !call["arguments"].is_object() {
                    return Err(GatewayError::with_status(
                        502,
                        "豆包工作返回未声明的工具或无效参数",
                    ));
                }
                normalized.push(json!({"id":crate::ids::random_id("call"),"type":"function","function":{"name":name,"arguments":call["arguments"].to_string()}}));
            }
            return Ok(json!({"role":"assistant","content":null,"tool_calls":normalized}));
        }
        if self.forced {
            return Err(GatewayError::with_status(502, "豆包工作未按要求调用工具"));
        }
        Ok(json!({"role":"assistant","content":text}))
    }
}

pub fn content(body: &Value, bridge: &ToolBridge) -> Result<(String, Vec<String>), GatewayError> {
    if body["response_format"]
        .as_object()
        .is_some_and(|format| format.get("type").and_then(Value::as_str) != Some("text"))
    {
        return Err(unsupported("豆包工作暂不支持强制结构化输出"));
    }
    let messages = body["messages"]
        .as_array()
        .filter(|messages| !messages.is_empty())
        .ok_or_else(|| unsupported("messages 必须包含消息"))?;
    let mut history = messages.clone();
    let mut images = Vec::new();
    for message in &mut history {
        if !matches!(
            message["role"].as_str(),
            Some("system" | "developer" | "user" | "assistant" | "tool")
        ) {
            return Err(unsupported("豆包工作不支持该消息角色"));
        }
        if let Some(parts) = message["content"].as_array_mut() {
            for part in parts {
                match part["type"].as_str() {
                    Some("text") if part["text"].is_string() => {}
                    Some("image_url") => {
                        let url = part["image_url"]["url"]
                            .as_str()
                            .ok_or_else(|| unsupported("图片缺少 image_url.url"))?;
                        if !url.starts_with("data:image/") {
                            return Err(unsupported("豆包工作对话图片输入仅接受 base64 data URL"));
                        }
                        if images.len() >= 10 {
                            return Err(unsupported("豆包工作单次最多输入 10 张图片"));
                        }
                        images.push(url.into());
                        *part = json!({"type":"text","text":format!("[Attached image {}]",images.len())});
                    }
                    _ => return Err(unsupported("豆包工作不支持该消息内容块")),
                }
            }
        } else if !message["content"].is_string() && !message["content"].is_null() {
            return Err(unsupported("豆包工作消息内容格式无效"));
        }
    }
    let mut prompt = bridge.instruction();
    prompt.push_str(&json!(history).to_string());
    Ok((prompt, images))
}

fn text_block(text: &str) -> Value {
    json!({"block_type":10000,"content":{"text_block":{"text":text,"icon_url":"","icon_url_dark":"","summary":""},"pc_event_block":""},"block_id":crate::ids::uuid_v4(),"parent_id":"","meta_info":[],"append_fields":[]})
}

pub fn payload(
    bot: &str,
    model: &Model,
    prompt: &str,
    attachments: Vec<Value>,
    body: &Value,
) -> Result<Value, GatewayError> {
    let id = crate::ids::uuid_v4();
    let now = crate::http::now_ms();
    let mut blocks = Vec::new();
    if !attachments.is_empty() {
        blocks.push(json!({"block_type":10052,"content":{"attachment_block":{"attachments":attachments},"pc_event_block":""},"block_id":crate::ids::uuid_v4(),"parent_id":"","meta_info":[],"append_fields":[]}));
    }
    blocks.push(text_block(prompt));
    let digits: String = crate::ids::uuid_v4()
        .bytes()
        .filter(|byte| byte.is_ascii_hexdigit())
        .filter_map(|byte| (byte as char).to_digit(16))
        .map(|digit| char::from(b'0' + (digit % 10) as u8))
        .take(16)
        .collect();
    let local_id = format!("local_{digits}");
    let mut result = json!({"client_meta":{"local_conversation_id":local_id,"conversation_id":"","bot_id":bot,"last_section_id":"","last_message_index":null},"messages":[{"local_message_id":id,"content_block":blocks,"message_status":0}],"option":{"create_time_ms":now,"need_create_conversation":true,"conversation_init_option":{"need_ack_conversation":true},"need_deep_think":model.deep_think,"is_audio":false,"answer_with_suggest":false,"tts_switch":false,"is_regen":false,"is_replace":false,"unique_key":crate::ids::uuid_v4(),"start_seq":0,"sse_recv_event_options":{"support_chunk_delta":true},"recovery_option":{"is_recovery":false,"req_create_time_sec":now/1000,"append_sse_event_scene":0},"message_storage_type":0},"user_context":[],"ext":{"use_deep_think":model.deep_think.to_string(),"sub_conv_firstmet_type":"1","commerce_credit_config_enable":"0"}});
    if model.agent_mode != 0 || model.mode_id.is_some() {
        let effort = match body["reasoning_effort"].as_str() {
            None => None,
            Some("low") => Some(3),
            Some("medium") => Some(4),
            Some("high") => Some(5),
            Some("xhigh") => Some(6),
            Some("max") => Some(7),
            _ => return Err(unsupported("豆包工作不支持该思考档位")),
        };
        let task = json!({"action":0,"thread_local_message_id":[id],"client_option":{"enable_sandbox":false,"os":std::env::consts::OS,"shared_folder_path":[]},"runtime_type":2,"agent_task_param":{"runtime_type":2},"agent_task_param_change":{"runtime_changed":true,"device_changed":false,"sandbox_auth_type_changed":false},"need_modify_conversation":false,"task_input_json":"{\"project_context\":{}}"});
        result["option"]["agent_mode"] = json!(model.agent_mode);
        result["option"]["model_config"] =
            json!({"model_item_key":model.id,"model_extra_params":model.extra});
        result["option"]["aggregate_params"] = json!({"mode_id":model.mode_id.as_deref().unwrap_or("3"),"model_item_key":model.id,"agent_mode":model.agent_mode.to_string()});
        if let Some(provider) = model.extra["provider_id"].as_str() {
            result["option"]["aggregate_params"]["provider_id"] = json!(provider);
        }
        if let Some(effort) = effort {
            result["option"]["model_config"]["reasoning_effort"] = json!(effort);
            result["option"]["aggregate_params"]["reasoning_effort"] = json!(effort.to_string());
        }
        result["ext"]["agent_mode"] = json!(model.agent_mode.to_string());
        if matches!(model.agent_mode, 1 | 2) {
            result["option"]["general_task_param"] = task.clone();
            result["ext"]["general_task_param"] = json!(task.to_string());
            result["ext"]["is_finish"] = json!("1");
        }
    } else if !body["reasoning_effort"].is_null() {
        return Err(unsupported("此豆包模式没有思考档位选择器"));
    }
    Ok(result)
}
