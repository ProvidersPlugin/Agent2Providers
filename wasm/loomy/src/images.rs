//! Loomy 生图及参考图编辑都调用 /images/generations，保留官方用途/追踪头。
//! 编辑将 images[].image_url 内联图转为 data URL 数组。大图入站仍受宿主 ABI 限制。
//! 上游默认签名 URL 由受控公共资源下载，无 token 的公开出口，不开放任意 HTTP。

use agent2api_wasm_sdk::{self as sdk, http, images::Resource, json, Error, Result, Value};
use base64::Engine;

use super::{account, errors, headers, models, MODEL_BASE};

const MAX_RESPONSE: usize = 8 * 1024 * 1024;

fn uncertain(message: &str) -> Error {
    Error::http(
        502,
        format!("Loomy 图片结果尚未确认：{message}；未自动重新提交，以免重复消耗积分"),
    )
    .with_code("image_output_uncertain")
}

pub fn generate(mut params: Value, editing: bool) -> Result<Value> {
    let record = account::current()?;
    let session = account::token(&record["secret"])?;
    let client_headers = params
        .as_object_mut()
        .and_then(|object| object.remove("headers"))
        .unwrap_or(Value::Null);
    let mut body = sdk::take_body(&mut params).unwrap_or(Value::Null);
    drop(params);
    if !body.is_object() {
        return Err(Error::http(400, "Loomy 图片请求体必须是 JSON 对象"));
    }
    let model = body["model"]
        .as_str()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| Error::http(400, "Loomy 图片请求缺少模型名称"))?
        .to_string();
    if models::entry(&model, &session)?.is_some_and(|entry| entry["kind"] != "image") {
        return Err(Error::http(400, "Loomy 图片生成必须使用 type=image 的模型"));
    }
    if editing {
        for field in ["mask", "input_fidelity"] {
            if body.get(field).is_some_and(|value| !value.is_null()) {
                return Err(Error::http(400, format!("Loomy 参考图编辑不支持 {field}")));
            }
        }
        let input = body["images"]
            .as_array()
            .filter(|images| !images.is_empty())
            .ok_or_else(|| Error::http(400, "图片编辑需要至少一张参考图"))?;
        let urls: Vec<Value> = input
            .iter()
            .map(|image| {
                image["image_url"]
                    .as_str()
                    .filter(|url| url.starts_with("data:image/"))
                    .map(|url| json!(url))
                    .ok_or_else(|| Error::http(400, "Loomy 图片编辑需要内联的参考图"))
            })
            .collect::<Result<_>>()?;
        body["images"] = json!(urls);
    }
    if let Some(object) = body.as_object_mut() {
        for field in [
            "response_format",
            "output_format",
            "output_compression",
            "stream",
            "partial_images",
            "mask",
            "input_fidelity",
        ] {
            object.remove(field);
        }
        if object
            .get("size")
            .is_some_and(|value| value.is_null() || value == "auto")
        {
            object.remove("size");
        }
    }
    account::ensure_current(&record)?;
    let response = http::open(
        "POST",
        &format!("{MODEL_BASE}/images/generations"),
        headers::build(&session, &client_headers, true)?,
        Some(&body),
    )
    .map_err(|_| uncertain("请求传输失败"))?;
    drop(client_headers);
    drop(body);
    let status = response.status;
    let bytes = response
        .bytes(MAX_RESPONSE)
        .map_err(|error| uncertain(&error.message))?;
    account::ensure_current(&record)?;
    let payload: Value =
        serde_json::from_slice(&bytes).map_err(|_| uncertain("上游没有返回有效 JSON"))?;
    if !(200..300).contains(&status) || payload.get("error").is_some_and(|error| !error.is_null()) {
        return Err(errors::http_error(status, &payload, &[&session]));
    }
    let code = errors::business_code(&payload);
    if !code.is_empty() && code != errors::SUCCESS {
        return Err(errors::http_error(status, &payload, &[&session]));
    }
    let rows = payload["data"]
        .as_array()
        .filter(|rows| !rows.is_empty() && rows.len() <= 16)
        .ok_or_else(|| uncertain("上游未返回有效图片数组"))?;
    let mut data = Vec::with_capacity(rows.len());
    for row in rows {
        account::ensure_current(&record)?;
        let mut image = if let Some(url) = row["url"].as_str().filter(|url| !url.is_empty()) {
            Resource::fetch_public(url)
                .and_then(Resource::image_result)
                .map_err(|_| uncertain("生成图片未能安全下载"))?
        } else if let Some(encoded) = row["b64_json"].as_str().filter(|value| !value.is_empty()) {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| uncertain("图片内容不是有效 base64"))?;
            let resource = Resource::create(
                mime(&bytes).ok_or_else(|| uncertain("图片格式不是 PNG、JPEG 或 WebP"))?,
            )?;
            resource.append(&bytes)?;
            resource.image_result()?
        } else {
            return Err(uncertain("图片条目缺少 url 或 b64_json"));
        };
        if let Some(prompt) = row.get("revised_prompt").filter(|value| value.is_string()) {
            image["revised_prompt"] = prompt.clone();
        }
        data.push(image);
    }
    let mut result = json!({ "created": payload["created"].as_i64().unwrap_or_else(|| sdk::now() / 1000), "data": data });
    if let Some(usage) = payload.get("usage") {
        result["usage"] = usage.clone();
    }
    if let Some(points) = payload.get("points_consumed").filter(|value| {
        value
            .as_f64()
            .is_some_and(|value| value.is_finite() && value >= 0.0)
    }) {
        result["points_consumed"] = points.clone();
    }
    account::ensure_current(&record)?;
    Ok(result)
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
