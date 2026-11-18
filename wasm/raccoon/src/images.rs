//! 1.1.5图片协议：gen发送JSON(gpt-image-1)，edits重复image文件字段，公开URL交宿主下载。
//! 内联入站仍受16MiB ABI限制；这里只把已收到的小图转受控资源，不宣称解决大图入站。

use super::{endpoints, errors, image_size, network, session};
use agent2api_wasm_sdk::{self as sdk, http, images::Resource, json, Error, Result, Value};
use base64::Engine;

pub const MODEL: &str = "raccoon-image";
pub fn model() -> Value {
    json!({ "id": MODEL, "name": "Raccoon Image", "kind": "image", "synthetic": true,
        "output_modalities": ["image"], "supportsImages": true, "supports_image_generation": true,
        "supports_image_editing": true, "supportsToolCall": false, "supportsReasoning": false, "isDefault": false })
}
fn validate(body: &Value) -> Result<()> {
    if !body["model"]
        .as_str()
        .is_some_and(|model| model.eq_ignore_ascii_case(MODEL))
    {
        return Err(Error::http(400, "小浣熊图片接口请使用raccoon-image"));
    }
    if body.get("n").is_some_and(|n| n.as_u64() != Some(1)) {
        return Err(Error::http(400, "小浣熊图片单次仅支持n=1"));
    }
    for (key, allowed) in [
        ("quality", &["auto", "standard"][..]),
        ("background", &["auto", "opaque"][..]),
        ("moderation", &["auto"][..]),
    ] {
        if body.get(key).is_some_and(|value| {
            !value.is_null() && !value.as_str().is_some_and(|value| allowed.contains(&value))
        }) {
            return Err(Error::http(400, format!("小浣熊图片接口不支持该{key}值")));
        }
    }
    for field in ["mask", "input_fidelity"] {
        if body.get(field).is_some_and(|value| !value.is_null()) {
            return Err(Error::http(400, format!("小浣熊参考图编辑不支持{field}")));
        }
    }
    Ok(())
}
fn prompt(body: &Value) -> Result<String> {
    let prompt = body["prompt"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::http(400, "图片请求缺少prompt"))?;
    let mut parts = vec![prompt.to_string()];
    for (field, label) in [("style", "Style"), ("negative_prompt", "Avoid")] {
        if let Some(value) = body[field]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            parts.push(format!("{label}: {value}"));
        }
    }
    Ok(parts.join("\n"))
}
fn mime(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("image/png", "png"))
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some(("image/jpeg", "jpg"))
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP".as_slice()) {
        Some(("image/webp", "webp"))
    } else {
        None
    }
}
fn files(body: &Value) -> Result<Vec<Value>> {
    let images = body["images"]
        .as_array()
        .filter(|images| !images.is_empty() && images.len() <= 16)
        .ok_or_else(|| Error::http(400, "小浣熊编辑需要1至16张内联参考图"))?;
    images.iter().enumerate().map(|(index, image)| {
        let value = image["image_url"].as_str().ok_or_else(|| Error::http(400, "参考图缺少image_url"))?;
        let (declared, encoded) = value.strip_prefix("data:").and_then(|value| value.split_once(";base64,"))
            .ok_or_else(|| Error::http(400, "小浣熊参考图需要内联data URL"))?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).map_err(|_| Error::http(400, "参考图base64无效"))?;
        let (mime, extension) = mime(&bytes).ok_or_else(|| Error::http(400, "小浣熊参考图只支持PNG、JPEG或WebP"))?;
        if declared != mime && !(declared == "image/jpg" && mime == "image/jpeg") { return Err(Error::http(400, "参考图声明格式与图片内容不符")); }
        let resource = Resource::create(mime)?; resource.append(&bytes)?; resource.seal()?;
        Ok(json!({ "field": "image", "filename": format!("reference-{}.{extension}", index + 1), "content_type": mime, "resource": resource.id }))
    }).collect()
}

pub fn generate(params: &Value, edit: bool) -> Result<Value> {
    let body = &params["body"];
    validate(body)?;
    let prompt = prompt(body)?;
    let size = image_size::normalize(body)?;
    let session = session::ensure(false)?;
    let credential = &session.credential;
    let input_files = if edit { files(body)? } else { Vec::new() };
    let input_resources: Vec<String> = input_files
        .iter()
        .filter_map(|file| file["resource"].as_str().map(str::to_string))
        .collect();
    let response = if edit {
        http::multipart(
            "POST",
            &network::url(&format!("{}/images/edits", endpoints::LLM))?,
            network::headers(
                &credential.token,
                &credential.org,
                json!({ "Accept": "application/json" }),
            )?,
            json!({ "prompt": prompt, "size": size }),
            input_files,
        )
    } else {
        network::open(
            "POST",
            &format!("{}/images/gen", endpoints::LLM),
            &credential.token,
            &credential.org,
            json!({ "Accept": "application/json" }),
            Some(&json!({ "model": "gpt-image-1", "prompt": prompt, "size": size })),
        )
    }
    .map_err(|error| {
        if error.code.as_deref() == Some("forbidden") {
            error
        } else {
            errors::image_uncertain("请求传输失败")
        }
    })?;
    let reply = network::read(response, 8 * 1024 * 1024)
        .map_err(|_| errors::image_uncertain("无法读取有效响应"))?;
    if errors::failed(reply.status, &reply.value) {
        return Err(errors::upstream(
            reply.status,
            &reply.value,
            &[&credential.token, &credential.refresh],
        ));
    }
    let rows = reply.value["data"]
        .as_array()
        .filter(|rows| !rows.is_empty() && rows.len() <= 16)
        .ok_or_else(|| errors::image_uncertain("上游未返回有效图片数组"))?;
    // 宿主同次最多保留16个资源；上传完成后释放参考图槽位，16图编辑也必须能接收结果。
    for id in input_resources {
        Resource { id }.remove()?;
    }
    let mut images = Vec::new();
    for row in rows {
        let resource = if let Some(url) = row["url"].as_str().filter(|url| !url.is_empty()) {
            Resource::fetch_public(url).map_err(|_| errors::image_uncertain("图片未能安全下载"))?
        } else if let Some(encoded) = row["b64_json"].as_str().filter(|value| !value.is_empty()) {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| errors::image_uncertain("图片base64无效"))?;
            let (mime, _) =
                mime(&bytes).ok_or_else(|| errors::image_uncertain("图片不是PNG、JPEG或WebP"))?;
            let resource = Resource::create(mime)?;
            resource.append(&bytes)?;
            resource
        } else {
            return Err(errors::image_uncertain("图片条目缺少url或b64_json"));
        };
        let mut value = resource.image_result()?;
        if let Some(prompt) = row["revised_prompt"].as_str() {
            value["revised_prompt"] = json!(prompt);
        }
        images.push(value);
    }
    let mut result = json!({ "created": reply.value["created"].as_i64().unwrap_or_else(|| sdk::now() / 1000), "data": images });
    if let Some(usage) = reply.value.get("usage") {
        result["usage"] = usage.clone();
    }
    Ok(result)
}
