//! 官方 1.1.5 的 /images/gen 为 JSON，/images/edits 使用重复 image 字段上传参考图。

use agent2api_provider_sdk::images::{ImagePlan, ImageRequest, MultipartFile};
use agent2api_provider_sdk::{control_error, unsupported, PluginCtx};
use serde_json::{json, Value};

pub const MODEL_ID: &str = "raccoon-image";

pub fn model() -> Value {
    json!({
        "id": MODEL_ID,
        "name": "Raccoon Image",
        "kind": "image",
        "synthetic": true,
        "output_modalities": ["image"],
        "supportsImages": true,
        "supports_image_generation": true,
        "supports_image_editing": true,
        "supportsToolCall": false,
        "supportsReasoning": false,
        "isDefault": false,
    })
}

fn validate(req: &ImageRequest) -> Result<(), String> {
    if !req.wire_model.eq_ignore_ascii_case(MODEL_ID) {
        return Err(control_error(
            400,
            "小浣熊图片接口请使用 raccoon-image",
            None,
            None,
        ));
    }
    if req.body.get("n").and_then(Value::as_u64).unwrap_or(1) != 1 {
        return Err(control_error(
            400,
            "小浣熊图片接口单次仅支持 n=1",
            None,
            None,
        ));
    }
    for (field, defaults) in [
        ("quality", &["auto", "standard"][..]),
        ("background", &["auto", "opaque"][..]),
        ("moderation", &["auto"][..]),
    ] {
        if let Some(value) = req.body.get(field).filter(|value| !value.is_null()) {
            if !value
                .as_str()
                .is_some_and(|value| defaults.contains(&value))
            {
                return Err(control_error(
                    400,
                    format!("小浣熊图片接口不支持 {field}={value}"),
                    None,
                    None,
                ));
            }
        }
    }
    for field in ["mask", "input_fidelity"] {
        if req.body.get(field).is_some_and(|value| !value.is_null()) {
            return Err(control_error(
                400,
                format!("小浣熊参考图编辑不支持 {field}"),
                None,
                None,
            ));
        }
    }
    Ok(())
}

fn prompt(body: &Value) -> Result<String, String> {
    let prompt = body
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| control_error(400, "图片请求缺少 prompt", None, None))?;
    let mut parts = vec![prompt.to_string()];
    for (field, label) in [("style", "Style"), ("negative_prompt", "Avoid")] {
        if let Some(text) = body
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            parts.push(format!("{label}: {text}"));
        }
    }
    Ok(parts.join("\n"))
}

async fn headers(ctx: &PluginCtx, account: &str) -> Result<Vec<(String, String)>, String> {
    let (token, org_code) = super::request_identity(ctx, account).await?;
    if token.is_empty() {
        return Err(control_error(
            401,
            "小浣熊账号缺少 accessToken，无法生图",
            None,
            None,
        ));
    }
    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "application/json".to_string()),
        ("Authorization".to_string(), format!("Bearer {token}")),
    ];
    super::headers::append_org(&mut headers, &org_code)?;
    Ok(headers)
}

pub async fn build(ctx: &PluginCtx, req: ImageRequest) -> Result<ImagePlan, String> {
    validate(&req)?;
    Ok(ImagePlan::openai(
        format!("{}/images/gen", super::llm_base_url()),
        headers(ctx, &req.account).await?,
        json!({
            "model": "gpt-image-1",
            "prompt": prompt(&req.body)?,
            "size": super::image_size::normalize(&req.body)?,
        }),
    ))
}

pub async fn build_edit(ctx: &PluginCtx, req: ImageRequest) -> Result<ImagePlan, String> {
    validate(&req)?;
    if !ctx.host_supports_multipart().await {
        return Err(unsupported(
            "当前网关未支持参考图文件上传，请更新网关后再使用小浣熊图片编辑",
        ));
    }
    let images = req
        .body
        .get("images")
        .and_then(Value::as_array)
        .filter(|images| !images.is_empty() && images.len() <= 16)
        .ok_or_else(|| control_error(400, "小浣熊参考图编辑需要 1 至 16 张图片", None, None))?;
    let files = images
        .iter()
        .enumerate()
        .map(|(index, image)| {
            let value = image
                .get("image_url")
                .and_then(Value::as_str)
                .ok_or_else(|| control_error(400, "参考图缺少 image_url", None, None))?;
            let (mime, encoded) = value
                .strip_prefix("data:")
                .and_then(|value| value.split_once(";base64,"))
                .ok_or_else(|| control_error(400, "小浣熊参考图需要内联 data URL", None, None))?;
            let extension = match mime {
                "image/png" => "png",
                "image/jpeg" | "image/jpg" => "jpg",
                "image/webp" => "webp",
                _ => {
                    return Err(control_error(
                        400,
                        "小浣熊参考图仅支持 PNG、JPEG 或 WebP",
                        None,
                        None,
                    ))
                }
            };
            Ok(MultipartFile {
                field: "image".to_string(),
                filename: format!("reference-{}.{}", index + 1, extension),
                content_type: mime.to_string(),
                base64: encoded.to_string(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(ImagePlan::openai_multipart(
        format!("{}/images/edits", super::llm_base_url()),
        headers(ctx, &req.account).await?,
        json!({ "prompt": prompt(&req.body)?, "size": super::image_size::normalize(&req.body)? }),
        files,
    ))
}
