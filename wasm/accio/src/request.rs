//! Accio ADK 请求不是标准 Gemini generateContent，按原生 protobuf-JSON 信封构造。
//! system_instruction、parameters_json、args_json、response_json、tool_config 都必须保留字符串语义。
//! 图片 inline_data/file_data 直接交上游；没有已确认的 Accio 文件上传接口，不编造 multipart。

use crate::{credentials::Credentials, endpoints, reasoning, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::collections::HashMap;
use url::Url;

const MAX_JSON: usize = 10 * 1024 * 1024;

fn bad(message: &str) -> Error {
    Error::http(400, message)
}
fn name(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

fn image_url(url: &str) -> Result<Value> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, data) = rest
            .split_once(',')
            .ok_or_else(|| bad("Accio 图片 data URL 无效"))?;
        let mut fields = meta.split(';');
        let mime = fields.next().unwrap_or("");
        if !mime.starts_with("image/")
            || mime.len() > 64
            || !mime.bytes().all(|byte| byte.is_ascii_graphic())
            || !fields.any(|field| field == "base64")
            || data.is_empty()
            || data.len() > MAX_JSON
        {
            return Err(bad(
                "Accio 内联图片需要有效的 image/*;base64 内容，且仍受宿主消息大小限制",
            ));
        }
        for (index, chunk) in data.as_bytes().chunks(64 * 1024).enumerate() {
            STANDARD
                .decode(chunk)
                .map_err(|_| bad("Accio 图片 base64 无效"))?;
            if (index + 1) * 64 * 1024 < data.len() && chunk.contains(&b'=') {
                return Err(bad("Accio 图片 base64 中间出现结束填充"));
            }
        }
        return Ok(json!({ "inline_data": { "mime_type": mime, "data": data } }));
    }
    let parsed = Url::parse(url).map_err(|_| bad("Accio 图片 URL 无效"))?;
    if url.len() > 8192
        || !matches!(parsed.scheme(), "https" | "http")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
        || url.split_once("://").is_none_or(|(_, rest)| {
            rest.split(['/', '?', '#'])
                .next()
                .is_none_or(|host| host.is_empty() || host.contains('@'))
        })
        || url.chars().any(|ch| ch.is_control() || ch.is_whitespace())
        || url.contains('\\')
    {
        return Err(bad("Accio 远程图片只接受无登录凭据的 HTTP(S) URL"));
    }
    // 沿用原生 file_data 形状；URL 由上游读取，不使用本插件账号下载任意地址。
    Ok(json!({ "file_data": { "mime_type": "image/png", "file_uri": url } }))
}

fn signature(value: &Value) -> Result<Option<String>> {
    let found = [
        "/extra_content/google/thought_signature",
        "/thought_signature",
        "/thoughtSignature",
        "/signature",
    ]
    .iter()
    .find_map(|key| value.pointer(key).filter(|value| !value.is_null()));
    match found {
        None => Ok(None),
        Some(Value::String(text)) if text.is_empty() => Ok(None),
        Some(Value::String(text))
            if text.len() <= 64 * 1024 && !text.chars().any(char::is_control) =>
        {
            Ok(Some(text.clone()))
        }
        _ => Err(bad("Accio 思考签名格式无效")),
    }
}

fn content(value: &Value) -> Result<Vec<Value>> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::String(text) => Ok(if text.is_empty() {
            Vec::new()
        } else {
            vec![json!({ "text": text })]
        }),
        Value::Array(items) => {
            let mut parts = Vec::new();
            for item in items {
                match item["type"].as_str() {
                    Some("text" | "input_text" | "output_text") => {
                        let text = item["text"]
                            .as_str()
                            .ok_or_else(|| bad("Accio 文本内容块格式无效"))?;
                        if !text.is_empty() {
                            let mut part = json!({ "text": text });
                            if item["thought"] == true {
                                part["thought"] = json!(true);
                            }
                            if let Some(sig) = signature(item)? {
                                part["thought_signature"] = json!(sig);
                            }
                            parts.push(part);
                        }
                    }
                    Some("image_url" | "input_image") => {
                        let image = item.get("image_url").unwrap_or(item);
                        let url = image
                            .as_str()
                            .or_else(|| image["url"].as_str())
                            .ok_or_else(|| bad("Accio 图片缺少 URL"))?;
                        parts.push(image_url(url)?);
                    }
                    Some("image") if item["source"]["type"] == "base64" => {
                        let mime = item["source"]["media_type"].as_str().unwrap_or("image/png");
                        let data = item["source"]["data"]
                            .as_str()
                            .ok_or_else(|| bad("Accio 图片缺少 base64"))?;
                        parts.push(image_url(&format!("data:{mime};base64,{data}"))?);
                    }
                    Some("thinking") => {
                        let text = item["thinking"]
                            .as_str()
                            .ok_or_else(|| bad("Accio 思考内容格式无效"))?;
                        if !text.is_empty() {
                            let mut part = json!({ "text": text, "thought": true });
                            if let Some(sig) = signature(item)? {
                                part["thought_signature"] = json!(sig);
                            }
                            parts.push(part);
                        }
                    }
                    Some("image_resource") => {
                        return Err(Error::http(
                            503,
                            "Accio 大图片资源引用尚待统一入站契约，不能用任意资源标识替代图片",
                        ))
                    }
                    _ => return Err(bad("Accio 对话仅接受已适配的文本和图片内容块")),
                }
            }
            Ok(parts)
        }
        _ => Err(bad("Accio 消息内容必须是文本或内容块数组")),
    }
}

fn push(contents: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    if let Some(last) = contents.last_mut().filter(|last| last["role"] == role) {
        if let Some(existing) = last["parts"].as_array_mut() {
            existing.extend(parts);
            return;
        }
    }
    contents.push(json!({ "role": role, "parts": parts }));
}

pub fn build(body: &Value, entry: &Value, credential: &Credentials) -> Result<Value> {
    let messages = body["messages"]
        .as_array()
        .filter(|messages| !messages.is_empty())
        .ok_or_else(|| bad("Accio 对话缺少 messages"))?;
    if body
        .get("response_format")
        .is_some_and(|format| !format.is_null() && format["type"] != "text")
    {
        return Err(bad(
            "Accio ADK 尚未确认强制 JSON 输出协议，请使用普通文本输出",
        ));
    }
    if body["functions"]
        .as_array()
        .is_some_and(|items| !items.is_empty())
        || body
            .get("function_call")
            .is_some_and(|value| !value.is_null() && value != "none")
    {
        return Err(bad(
            "Accio 请使用 tools/tool_calls 形式，旧 functions 协议未适配",
        ));
    }
    if body
        .get("tools")
        .is_some_and(|tools| !tools.is_null() && !tools.is_array())
    {
        return Err(bad("Accio tools 必须为数组"));
    }
    if body["tools"]
        .as_array()
        .is_some_and(|tools| tools.len() > 256)
    {
        return Err(bad("Accio 单次最多声明 256 个工具"));
    }
    let choice = body.get("tool_choice").filter(|value| !value.is_null());
    if choice.is_some_and(|value| value != "none" && value != "auto") {
        return Err(bad(
            "Accio ADK 尚未确认强制指定工具协议，请使用自动工具选择",
        ));
    }
    let mut contents = Vec::new();
    let mut system = Vec::new();
    let mut names = HashMap::<String, String>::new();
    for message in messages {
        let role = message["role"].as_str().unwrap_or("user");
        match role {
            "system" | "developer" => {
                for part in content(&message["content"])? {
                    if let Some(text) = part["text"].as_str() {
                        system.push(text.to_string());
                    } else {
                        return Err(bad("Accio 系统消息不接受图片"));
                    }
                }
            }
            "assistant" => {
                let mut parts = content(&message["content"])?;
                if !parts.iter().any(|part| part["thought"] == true) {
                    if let Some(trace) = message["reasoning_content"]
                        .as_str()
                        .filter(|text| !text.is_empty())
                    {
                        parts.insert(0, json!({ "text": trace, "thought": true }));
                    }
                }
                if message
                    .get("tool_calls")
                    .is_some_and(|value| !value.is_null() && !value.is_array())
                {
                    return Err(bad("Accio 历史工具调用必须为数组"));
                }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    if call.get("type").is_some_and(|value| value != "function") {
                        return Err(bad("Accio 工具历史只能包含函数调用"));
                    }
                    let id = call["id"]
                        .as_str()
                        .filter(|value| {
                            !value.is_empty()
                                && value.len() <= 512
                                && !value.chars().any(char::is_control)
                        })
                        .ok_or_else(|| bad("Accio 工具历史缺少有效调用标识"))?;
                    let tool = call["function"]["name"]
                        .as_str()
                        .filter(|value| name(value))
                        .ok_or_else(|| bad("Accio 工具历史缺少有效工具名"))?;
                    let args = call["function"]["arguments"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .unwrap_or("{}");
                    let arguments: Value = serde_json::from_str(args)
                        .map_err(|_| bad("Accio 历史工具参数不是完整 JSON"))?;
                    if !arguments.is_object() {
                        return Err(bad("Accio 历史工具参数必须为 JSON 对象"));
                    }
                    if names.insert(id.to_string(), tool.to_string()).is_some() {
                        return Err(bad("Accio 工具历史存在重复调用标识"));
                    }
                    let mut part =
                        json!({ "function_call": { "id": id, "name": tool, "args_json": args } });
                    if let Some(sig) = signature(call)? {
                        part["thought_signature"] = json!(sig);
                    }
                    parts.push(part);
                }
                push(&mut contents, "model", parts);
            }
            "tool" | "function" => {
                let id = message["tool_call_id"]
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| bad("Accio 工具结果缺少调用标识"))?;
                let explicit = message["name"].as_str().unwrap_or("");
                let known = names.get(id).map(String::as_str).unwrap_or("");
                if !explicit.is_empty() && !known.is_empty() && explicit != known {
                    return Err(bad("Accio 工具结果名称与历史调用不一致"));
                }
                let tool = if explicit.is_empty() { known } else { explicit };
                if !name(tool) {
                    return Err(bad("Accio 工具结果无法匹配具名调用，请保留工具历史"));
                }
                let text = message["content"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        if message["content"].is_null() {
                            String::new()
                        } else {
                            message["content"].to_string()
                        }
                    });
                let response = json!({ "content": text, "is_error": message["is_error"].as_bool().unwrap_or(false) });
                push(
                    &mut contents,
                    "tool",
                    vec![
                        json!({ "function_response": { "id": id, "name": tool, "response_json": response.to_string() } }),
                    ],
                );
            }
            "user" => push(&mut contents, "user", content(&message["content"])?),
            _ => return Err(bad("Accio 不支持该消息角色")),
        }
    }
    if contents.is_empty() {
        return Err(bad("Accio 对话没有有效用户消息"));
    }
    let images = contents
        .iter()
        .flat_map(|item| item["parts"].as_array().into_iter().flatten())
        .any(|part| part.get("inline_data").is_some() || part.get("file_data").is_some());
    if images && entry["supportsImages"] != true {
        return Err(bad("当前 Accio 国际版模型未声明图片输入能力"));
    }
    let model = entry["upstreamKey"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::http(502, "Accio 目录缺少上游模型标识"))?;
    let request_id = util::uuid()?;
    let mut output = json!({ "model": model, "tenant": endpoints::TENANT, "iai_tag": endpoints::IAI_TAG,
        "empid": "", "request_id": request_id, "message_id": format!("msg-{request_id}"), "token": credential.access,
        "contents": contents, "system_instruction": system.join("\n\n"), "properties": { "normalized_response": "true" } });
    let mut tools = Vec::new();
    if !choice.is_some_and(|value| value == "none") {
        for tool in body["tools"].as_array().into_iter().flatten() {
            if tool.get("type").is_some_and(|kind| kind != "function") {
                return Err(bad("Accio 只支持函数工具声明"));
            }
            let function = tool.get("function").unwrap_or(tool);
            let tool_name = function["name"]
                .as_str()
                .filter(|value| name(value))
                .ok_or_else(|| bad("Accio 工具名称无效"))?;
            let parameters = function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
            if !parameters.is_object() {
                return Err(bad("Accio 工具参数声明必须为 JSON 对象"));
            }
            tools.push(json!({ "name": tool_name, "description": function["description"].as_str().unwrap_or(""), "parameters_json": parameters.to_string() }));
        }
    }
    if !tools.is_empty() {
        output["tools"] = json!(tools);
        output["tool_config"] = json!(
            json!({ "functionCallingConfig": { "streamFunctionCallArguments": true } }).to_string()
        );
    }
    for field in ["temperature", "top_p"] {
        if let Some(value) = body[field].as_f64().filter(|value| value.is_finite()) {
            output[field] = json!(value);
        }
    }
    if let Some(value) = body
        .get("max_tokens")
        .or_else(|| body.get("max_completion_tokens"))
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
    {
        output["max_output_tokens"] = json!(value);
    }
    if let Some(stop) = body.get("stop").filter(|value| !value.is_null()) {
        output["stop_sequences"] = match stop {
            Value::String(text) => json!([text]),
            Value::Array(items) if items.iter().all(Value::is_string) => json!(items),
            _ => return Err(bad("Accio stop 必须为字符串或字符串数组")),
        };
    }
    if let Some(effort) = reasoning::resolve(body, entry) {
        output["include_thoughts"] = json!(true);
        if entry["protocolKind"] == "properties" {
            output["properties"]["reasoning_effort"] = json!(effort);
        } else {
            output["reasoning_effort"] = json!(effort);
        }
    }
    // body 再 base64 封装后必须小于 16 MiB；不是图片资源化入站的替代品。
    if output.to_string().len() > MAX_JSON {
        return Err(bad(
            "Accio 含图片的上游请求超过 10 MiB，受宿主 16 MiB 消息封装限制",
        ));
    }
    Ok(output)
}
