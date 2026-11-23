//! 官方图片分三档JSON请求；编辑图片为data URL数组，不是multipart。
//! 编辑正文分片写调用资源，避免二次base64信封；标准入站14MiB及RPC16MiB限制不变。
mod request_body;

use super::{catalog, errors, headers, image_models, network, session};
use agent2api_wasm_sdk::{self as sdk, images::Resource, json, Error, Result, Value};
use base64::Engine;

fn uncertain() -> Error {
    Error::http(502, "WorkBuddy图片结果未确认，未自动重新提交以免重复扣费")
        .with_code("image_output_uncertain")
}
fn mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP".as_slice()) {
        Some("image/webp")
    } else {
        None
    }
}
pub fn generate(params: &Value, editing: bool) -> Result<Value> {
    let original = &params["body"];
    let model = original["model"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::http(400, "图片请求缺少model"))?;
    let current = session::ensure(false)?;
    let declared = catalog::check(&current, model)?;
    if declared.as_ref().is_some_and(|row| row["kind"] != "image") {
        return Err(Error::http(
            400,
            "该WorkBuddy模型属于对话通道，请选择已声明的图片模型",
        ));
    }
    let (generation, editable) = declared
        .as_ref()
        .and_then(image_models::capabilities)
        .unwrap_or_else(|| {
            let editing = model.to_ascii_lowercase().contains("edit");
            (!editing, editing)
        });
    if editing && !editable || !editing && !generation {
        return Err(Error::http(400, "该WorkBuddy图片模型不支持所选操作"));
    }
    let prompt = original["prompt"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::http(400, "图片请求缺少prompt"))?;
    let size = original["size"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("auto"))
        .unwrap_or("1024x1024");
    let mut body = json!({ "model": model, "prompt": prompt, "size": size });
    let hunyuan = model.starts_with("hunyuan-");
    let gemini = model.starts_with("gemini-");
    if (hunyuan || gemini)
        && ["quality", "style", "background", "moderation"]
            .iter()
            .any(|key| original.get(*key).is_some_and(|v| !v.is_null()))
    {
        return Err(Error::http(
            400,
            "此WorkBuddy图片模型不接收质量、风格或背景参数，请写在提示词中",
        ));
    }
    if original.get("mask").is_some_and(|v| !v.is_null()) {
        return Err(Error::http(400, "WorkBuddy图片编辑未声明蒙版支持"));
    }
    if let Some(n) = original.get("n") {
        if !n.as_u64().is_some_and(|n| (1..=16).contains(&n)) {
            return Err(Error::http(400, "图片数量必须为1至16"));
        }
    }
    if gemini && original["n"].as_u64().is_some_and(|n| n != 1) {
        return Err(Error::http(400, "此WorkBuddy图片模型仅接受单图请求"));
    }
    if hunyuan {
        body["n"] = json!(original["n"].as_u64().unwrap_or(1));
        for key in ["footnote", "revise"] {
            if let Some(value) = original.get(key).filter(|v| !v.is_null()) {
                body[key] = value.clone();
            }
        }
    } else if !gemini {
        body["response_format"] = json!("b64_json");
        for key in ["n", "quality", "style", "background"] {
            if let Some(value) = original.get(key).filter(|v| !v.is_null()) {
                body[key] = value.clone();
            }
        }
    }
    let references = if editing {
        let references = request_body::References::parse(&original["images"])?;
        // 保留原对象成员顺序，实际图片字符串由借用写出器在此键输出，不创建第二份数组。
        body["image"] = Value::Null;
        if let Some(value) = original.get("input_fidelity").filter(|v| !v.is_null()) {
            body["input_fidelity"] = value.clone();
        }
        Some(references)
    } else {
        None
    };
    let response = if let Some(references) = references {
        references.open(
            &current.endpoint()?,
            headers::framework(&current.secret)?,
            &body,
        )?
    } else {
        network::open(
            "POST",
            &current.endpoint()?,
            "/v2/images/generations",
            headers::framework(&current.secret)?,
            Some(&body),
        )
        .map_err(|_| uncertain())?
    };
    let reply = network::read(response, 8 * 1024 * 1024).map_err(|_| uncertain())?;
    if !reply.ok() {
        return Err(errors::upstream(
            reply.status,
            &reply.value,
            &current.secrets(),
        ));
    }
    let data = reply.data();
    let rows = data["data"]
        .as_array()
        .filter(|v| !v.is_empty() && v.len() <= 16)
        .ok_or_else(uncertain)?;
    let mut images = Vec::new();
    for row in rows {
        let resource = if let Some(url) = row["url"].as_str().filter(|s| !s.is_empty()) {
            Resource::fetch_public(url).map_err(|_| uncertain())?
        } else if let Some(encoded) = row["b64_json"].as_str() {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| uncertain())?;
            let resource =
                Resource::create(mime(&bytes).ok_or_else(uncertain)?).map_err(|_| uncertain())?;
            resource.append(&bytes).map_err(|_| uncertain())?;
            resource
        } else {
            return Err(uncertain());
        };
        let mut image = resource.image_result().map_err(|_| uncertain())?;
        if let Some(prompt) = row.get("revised_prompt") {
            image["revised_prompt"] = prompt.clone();
        }
        images.push(image);
    }
    let mut output = json!({ "created": data["created"].as_i64().unwrap_or_else(|| sdk::now() / 1000), "data": images });
    if let Some(usage) = data.get("usage") {
        output["usage"] = usage.clone();
    }
    Ok(output)
}
