//! 标准 OpenAI 图片历史在 Loomy 的 DeepSeek 路由需转换为客户端 image_base64 标记。
use agent2api_wasm_sdk::{json, Error, Result, Value};
use base64::Engine;

fn image(part: &Value) -> Result<Option<String>> {
    if !matches!(part["type"].as_str(), Some("image_url" | "input_image")) {
        return Ok(None);
    }
    let url = part["image_url"]
        .as_str()
        .or_else(|| part["image_url"]["url"].as_str())
        .or_else(|| part["url"].as_str())
        .ok_or_else(|| Error::http(400, "Loomy 图片消息缺少地址"))?;
    if !url.starts_with("data:") {
        return Ok(None);
    }
    let (prefix, encoded) = url
        .split_once(',')
        .ok_or_else(|| Error::http(400, "Loomy 内联图片格式不正确"))?;
    if !matches!(
        prefix,
        "data:image/png;base64" | "data:image/jpeg;base64" | "data:image/webp;base64"
    ) || encoded.len() > 4 * 1024 * 1024
        || base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .is_err()
    {
        return Err(Error::http(400, "Loomy 内联图片格式不支持或超过长度限制"));
    }
    Ok(Some(format!("<image_base64>{url}</image_base64>")))
}

pub fn adapt(body: &mut Value, model: &str) -> Result<()> {
    if !model.to_ascii_lowercase().contains("deepseek") {
        return Ok(());
    }
    let messages = body["messages"]
        .as_array_mut()
        .ok_or_else(|| Error::http(400, "Loomy 请求缺少消息列表"))?;
    for message in messages {
        let Some(parts) = message["content"].as_array_mut() else {
            continue;
        };
        let mut output: Vec<Value> = Vec::with_capacity(parts.len());
        for part in std::mem::take(parts) {
            if let Some(tag) = image(&part)? {
                if let Some(previous) = output.last_mut().filter(|part| part["type"] == "text") {
                    let text = previous["text"].as_str().unwrap_or("");
                    previous["text"] = json!(if text.is_empty() {
                        tag
                    } else {
                        format!("{text}\n{tag}")
                    });
                } else {
                    output.push(json!({ "type": "text", "text": tag }));
                }
            } else {
                output.push(part);
            }
        }
        *parts = output;
    }
    Ok(())
}
