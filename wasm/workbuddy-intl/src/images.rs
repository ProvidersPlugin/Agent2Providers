//! 官方图片分三档JSON请求；编辑图片为data URL数组，不是multipart。
//! 公网图片只经resources.fetch-public；大base64入站16MiB限制仍待宿主资源化。
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
    let declared = catalog::image(&current, model)?;
    let (generation, editable) = image_models::capabilities(&declared)
        .ok_or_else(|| Error::http(400, "国际目录没有声明该图片能力，未提交图片请求"))?;
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
    if editing {
        let images = original["images"]
            .as_array()
            .filter(|images| !images.is_empty() && images.len() <= 16)
            .ok_or_else(|| Error::http(400, "参考图需为1至16张"))?;
        let mut references = Vec::new();
        for image in images {
            let url = image["image_url"]
                .as_str()
                .ok_or_else(|| Error::http(400, "参考图缺少内联地址"))?;
            let (declared, encoded) = url
                .strip_prefix("data:")
                .and_then(|s| s.split_once(";base64,"))
                .ok_or_else(|| Error::http(400, "参考图必须是内联data URL"))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| Error::http(400, "参考图编码无效"))?;
            let actual =
                mime(&bytes).ok_or_else(|| Error::http(400, "参考图必须是PNG、JPEG或WebP"))?;
            if actual != declared && !(actual == "image/jpeg" && declared == "image/jpg") {
                return Err(Error::http(400, "参考图声明格式与实际内容不符"));
            }
            references.push(json!(url));
        }
        body["image"] = json!(references);
        if let Some(value) = original.get("input_fidelity").filter(|v| !v.is_null()) {
            body["input_fidelity"] = value.clone();
        }
    }
    let path = if editing {
        "/v2/images/edits"
    } else {
        "/v2/images/generations"
    };
    let response = network::open(
        "POST",
        &current.endpoint()?,
        path,
        headers::framework(&current.secret)?,
        Some(&body),
    )
    .map_err(|_| uncertain())?;
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
