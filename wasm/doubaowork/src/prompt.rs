use crate::{errors, models::Model, util};
use agent2api_wasm_sdk::{self as sdk, images::Resource, json, Error, Result, Value};
use base64::Engine;

pub struct Prepared {
    pub text: String,
    pub images: Vec<Resource>,
    pub bridge: sdk::tool_bridge::ToolBridge,
}

pub fn prepare(body: &Value) -> Result<Prepared> {
    for field in [
        "temperature",
        "top_p",
        "presence_penalty",
        "frequency_penalty",
        "stop",
        "seed",
        "max_tokens",
        "max_completion_tokens",
    ] {
        if body.get(field).is_some_and(|value| !value.is_null()) {
            return Err(unsupported(&format!(
                "豆包工作没有 {field} 的对应设置，请移除该参数"
            )));
        }
    }
    if body
        .get("n")
        .is_some_and(|value| !value.is_null() && value.as_u64() != Some(1))
    {
        return Err(unsupported("豆包工作一次只返回一个回答"));
    }
    if body
        .get("response_format")
        .is_some_and(|value| !value.is_null() && value["type"] != "text")
    {
        return Err(unsupported("豆包工作不支持强制结构化输出"));
    }
    let bridge = sdk::tool_bridge::ToolBridge::new(body)?;
    let mut history = body["messages"]
        .as_array()
        .filter(|messages| !messages.is_empty() && messages.len() <= 512)
        .cloned()
        .ok_or_else(|| errors::invalid("messages 必须是非空数组"))?;
    let mut images = Vec::new();
    let mut total = 0usize;
    for message in &mut history {
        if !matches!(
            message["role"].as_str(),
            Some("system" | "developer" | "user" | "assistant" | "tool")
        ) {
            return Err(unsupported("豆包工作不支持此消息角色"));
        }
        let user = message["role"] == "user";
        if let Some(parts) = message["content"].as_array_mut() {
            for part in parts {
                match part["type"].as_str() {
                    Some("text" | "output_text") if part["text"].is_string() => {}
                    Some("image_url" | "input_image" | "image") if user => {
                        if images.len() >= 10 {
                            return Err(unsupported("豆包工作一次最多接收 10 张图片"));
                        }
                        let owned;
                        let url = if part["type"] == "image" && part["source"]["type"] == "base64" {
                            owned = format!(
                                "data:{};base64,{}",
                                part["source"]["media_type"]
                                    .as_str()
                                    .ok_or_else(|| unsupported("图片格式缺失"))?,
                                part["source"]["data"]
                                    .as_str()
                                    .ok_or_else(|| unsupported("图片内容缺失"))?
                            );
                            &owned
                        } else {
                            part["image_url"]
                                .as_str()
                                .or_else(|| part["image_url"]["url"].as_str())
                                .or_else(|| part["url"].as_str())
                                .ok_or_else(|| unsupported("图片地址缺失"))?
                        };
                        let resource = if url.starts_with("https://") {
                            Resource::fetch_public(url)?
                        } else {
                            let (prefix, encoded) = url.split_once(',').ok_or_else(|| {
                                unsupported("豆包工作图片需使用内联数据或 HTTPS 公网图片")
                            })?;
                            let mime = match prefix {
                                "data:image/png;base64" => "image/png",
                                "data:image/jpeg;base64" | "data:image/jpg;base64" => "image/jpeg",
                                "data:image/webp;base64" => "image/webp",
                                _ => {
                                    return Err(unsupported("豆包工作图片只支持 PNG、JPEG 和 WebP"))
                                }
                            };
                            let bytes = base64::engine::general_purpose::STANDARD
                                .decode(encoded)
                                .map_err(|_| unsupported("图片 base64 无效"))?;
                            let resource = Resource::create(mime)?;
                            resource.append(&bytes)?;
                            resource.seal()?;
                            resource
                        };
                        let sealed = resource.seal()?;
                        let size = sealed["size"]
                            .as_u64()
                            .and_then(|size| usize::try_from(size).ok())
                            .ok_or_else(|| unsupported("图片资源大小无效"))?;
                        total = total.saturating_add(size);
                        if size == 0 || size > 20 * 1024 * 1024 || total > 64 * 1024 * 1024 {
                            return Err(unsupported("豆包工作图片超过大小限制"));
                        }
                        images.push(resource);
                        *part = json!({"type":"text","text":format!("[Attached image {}]",images.len())});
                    }
                    _ => return Err(unsupported("豆包工作不支持此消息内容块")),
                }
            }
        } else if !message["content"].is_string() && !message["content"].is_null() {
            return Err(unsupported("消息内容须为文本或内容块"));
        }
    }
    let text = format!("{}{}", bridge.instruction(), Value::Array(history));
    if text.len() > 8 * 1024 * 1024 {
        return Err(unsupported("豆包工作对话历史超过 8 MiB"));
    }
    Ok(Prepared {
        text,
        images,
        bridge,
    })
}

pub fn uuid() -> Result<String> {
    let raw = sdk::host("random.bytes", json!({"length":16}))?;
    let mut bytes = base64::engine::general_purpose::STANDARD
        .decode(raw.as_str().unwrap_or(""))
        .map_err(|_| errors::protocol("随机值无效"))?;
    if bytes.len() != 16 {
        return Err(errors::protocol("随机值长度无效"));
    }
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    Ok(format!("{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",bytes[0],bytes[1],bytes[2],bytes[3],bytes[4],bytes[5],bytes[6],bytes[7],bytes[8],bytes[9],bytes[10],bytes[11],bytes[12],bytes[13],bytes[14],bytes[15]))
}
fn block(kind: i64, content: Value) -> Result<Value> {
    Ok(
        json!({"block_type":kind,"content":content,"block_id":uuid()?,"parent_id":"","meta_info":[],"append_fields":[]}),
    )
}

pub fn payload(
    bot: &str,
    model: &Model,
    text: &str,
    attachments: Vec<Value>,
    body: &Value,
) -> Result<Value> {
    let deep_think =
        if model.agent_mode != 0 && model.id.starts_with("person-") && model.deep_think == 0 {
            10001
        } else {
            model.deep_think
        };
    let id = uuid()?;
    let now = util::now()?;
    let mut blocks = Vec::new();
    if !attachments.is_empty() {
        blocks.push(block(
            10052,
            json!({"attachment_block":{"attachments":attachments},"pc_event_block":""}),
        )?);
    }
    blocks.push(block(10000,json!({"text_block":{"text":text,"icon_url":"","icon_url_dark":"","summary":""},"pc_event_block":""}))?);
    let digits = uuid()?
        .bytes()
        .filter(|byte| byte.is_ascii_hexdigit())
        .filter_map(|byte| char::from(byte).to_digit(16))
        .map(|digit| char::from(b'0' + (digit % 10) as u8))
        .take(16)
        .collect::<String>();
    let mut value = json!({"client_meta":{"local_conversation_id":format!("local_{digits}"),"conversation_id":"","bot_id":bot,"last_section_id":"","last_message_index":null},
        "messages":[{"local_message_id":id,"content_block":blocks,"message_status":0}],"option":{"create_time_ms":now,"need_create_conversation":true,"conversation_init_option":{"need_ack_conversation":true},"need_deep_think":deep_think,"is_audio":false,"answer_with_suggest":false,"tts_switch":false,"is_regen":false,"is_replace":false,"unique_key":uuid()?,"start_seq":0,"sse_recv_event_options":{"support_chunk_delta":true},"recovery_option":{"is_recovery":false,"req_create_time_sec":now/1000,"append_sse_event_scene":0},"message_storage_type":0},
        "user_context":[],"ext":{"use_deep_think":deep_think.to_string(),"sub_conv_firstmet_type":"1","commerce_credit_config_enable":"0"}});
    if model.agent_mode != 0 || model.mode_id.is_some() {
        let effort = match body["reasoning_effort"].as_str() {
            None => model.reasoning_default,
            Some("low") => Some(3),
            Some("medium") => Some(
                if model.reasoning_levels.contains(&8) && !model.reasoning_levels.contains(&4) {
                    8
                } else {
                    4
                },
            ),
            Some("high") => Some(5),
            Some("xhigh") => Some(6),
            Some("max") => Some(7),
            _ => return Err(unsupported("豆包工作不支持此思考档位")),
        };
        if effort.is_some_and(|level| {
            !model.reasoning_levels.is_empty() && !model.reasoning_levels.contains(&level)
        }) {
            return Err(unsupported("此豆包模型的官方目录不支持所选思考档位"));
        }
        value["option"]["agent_mode"] = json!(model.agent_mode);
        value["option"]["model_config"] =
            json!({"model_item_key":model.id,"model_extra_params":model.extra});
        value["option"]["aggregate_params"] = json!({"mode_id":model.mode_id.as_deref().unwrap_or("3"),"model_item_key":model.id,"agent_mode":model.agent_mode.to_string()});
        if let Some(conversation_mode) = &model.conversation_mode {
            value["option"]["aggregate_params"]["conversation_mode"] = json!(conversation_mode);
        }
        if let Some(provider) = model.extra["provider_id"].as_str() {
            value["option"]["aggregate_params"]["provider_id"] = json!(provider);
        }
        if let Some(effort) = effort {
            value["option"]["model_config"]["reasoning_effort"] = json!(effort);
            value["option"]["aggregate_params"]["reasoning_effort"] = json!(effort.to_string());
        }
        value["ext"]["agent_mode"] = json!(model.agent_mode.to_string());
        if matches!(model.agent_mode, 1 | 2) {
            let platform = sdk::host("runtime.platform", json!({}))?;
            let os = match platform.as_str() {
                Some("win") => "windows",
                Some("mac") => "macos",
                Some("linux") => "linux",
                _ => "other",
            };
            let task = json!({"action":0,"thread_local_message_id":[id],"client_option":{"enable_sandbox":false,"os":os,"shared_folder_path":[]},"runtime_type":2,"agent_task_param":{"runtime_type":2},"agent_task_param_change":{"runtime_changed":true,"device_changed":false,"sandbox_auth_type_changed":false},"need_modify_conversation":false,"task_input_json":"{\"project_context\":{}}"});
            value["option"]["general_task_param"] = task.clone();
            value["ext"]["general_task_param"] = json!(task.to_string());
            value["ext"]["is_finish"] = json!("1");
        }
    } else if !body["reasoning_effort"].is_null() {
        return Err(unsupported("此豆包模式没有思考档位选择器"));
    }
    value["ext"]["conversation_init_option"] =
        json!(value["option"]["conversation_init_option"].to_string());
    Ok(value)
}
fn unsupported(message: &str) -> Error {
    Error::http(400, message).with_code("unsupported_parameter")
}
