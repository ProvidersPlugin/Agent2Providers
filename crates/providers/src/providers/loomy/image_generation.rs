//! Loomy 模型网关的 Images API。
//!
//! ── 与官方客户端对齐（Loomy 0.9.39 `electron/image/image-generation-service.js`）──
//! ```text
//!   POST {model_base}/images/generations
//!   headers: token + Authorization Bearer + Accept + X-Loomy-Request-Purpose=image.generate
//!            + traceparent / ChatId / MsgId / TurnId / InvokeOrigin
//!   body:    { model, prompt, n:1, size:"2304x1728", response_format:"url", images? }
//! ```
//! 文生图与图生图共用 `/images/generations`；参考图是 data URL 或 http(s) URL 字符串数组
//! （不是 OpenAI edits 的 `image`/`mask` multipart）。

use agent2api_provider_sdk::images::{ImagePlan, ImageRequest};
use agent2api_provider_sdk::{control_error, PluginCtx};
use serde_json::{json, Value};

/// 官方客户端 `DEFAULT_IMAGE_GENERATION_OPTIONS.size`
const DEFAULT_SIZE: &str = "2304x1728";

/// 官方客户端从不发送、上游也不认的 OpenAI Images 扩展字段。
const UNSUPPORTED_FIELDS: &[&str] = &[
    "quality",
    "style",
    "background",
    "moderation",
    "output_format",
    "output_compression",
    "stream",
    "partial_images",
    "mask",
    "input_fidelity",
];

fn reject_unsupported(body: &Value) -> Result<(), String> {
    for field in UNSUPPORTED_FIELDS {
        if body.get(*field).is_some_and(|value| !value.is_null()) {
            return Err(control_error(
                400,
                format!("Loomy 生图不支持 {field}，请把要求写进 prompt"),
                None,
                None,
            ));
        }
    }
    Ok(())
}

fn resolve_size(body: &Value) -> String {
    body.get("size")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|size| !size.is_empty() && !size.eq_ignore_ascii_case("auto"))
        .unwrap_or(DEFAULT_SIZE)
        .to_string()
}

fn is_inline_or_remote_image(url: &str) -> bool {
    let trimmed = url.trim();
    trimmed.starts_with("data:image/")
        || trimmed.to_ascii_lowercase().starts_with("http://")
        || trimmed.to_ascii_lowercase().starts_with("https://")
}

pub async fn build_edit(ctx: &PluginCtx, mut req: ImageRequest) -> Result<ImagePlan, String> {
    // 官方客户端的 _generateImageToImage 仍调用 generations，images 是 URL 字符串数组。
    for field in ["mask", "input_fidelity"] {
        if req.body.get(field).is_some_and(|value| !value.is_null()) {
            return Err(control_error(
                400,
                format!("Loomy 参考图编辑不支持 {field}"),
                None,
                None,
            ));
        }
    }
    let images = req
        .body
        .get("images")
        .and_then(Value::as_array)
        .filter(|images| !images.is_empty())
        .ok_or_else(|| control_error(400, "图片编辑需要至少一张参考图", None, None))?;
    let urls = images
        .iter()
        .map(|image| {
            image
                .get("image_url")
                .and_then(Value::as_str)
                .filter(|url| is_inline_or_remote_image(url))
                .map(|url| Value::String(url.trim().to_string()))
                .ok_or_else(|| {
                    control_error(
                        400,
                        "Loomy 图片编辑需要 data URL 或 http(s) 参考图",
                        None,
                        None,
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    req.body["images"] = Value::Array(urls);
    build(ctx, req).await
}

pub async fn build(ctx: &PluginCtx, req: ImageRequest) -> Result<ImagePlan, String> {
    let entry = super::models::list().into_iter().find(|item| {
        item.get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id.eq_ignore_ascii_case(&req.wire_model))
    });
    if entry
        .as_ref()
        .is_some_and(|item| item.get("kind").and_then(Value::as_str) != Some("image"))
    {
        return Err(control_error(
            400,
            "Loomy 图片生成必须使用 type=image 的模型",
            None,
            None,
        ));
    }
    reject_unsupported(&req.body)?;
    let prompt = req
        .body
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .ok_or_else(|| control_error(400, "图片请求缺少 prompt", None, None))?;
    let credentials = super::credentials::snapshot(ctx, &req.account).await?;
    let token = credentials.session;
    // 按官方客户端拼 body：固定 response_format=url（网关 image_output 再按客户端
    // 请求转成 b64_json / 本地 url）；size 缺省或 auto 时落到官方默认 2304x1728。
    let mut body = json!({
        "model": req.wire_model,
        "prompt": prompt,
        "n": req.body.get("n").and_then(Value::as_u64).unwrap_or(1),
        "size": resolve_size(&req.body),
        "response_format": "url",
    });
    if let Some(images) = req
        .body
        .get("images")
        .and_then(Value::as_array)
        .filter(|images| !images.is_empty())
    {
        body["images"] = Value::Array(images.clone());
    }
    Ok(ImagePlan::openai(
        format!("{}/images/generations", super::endpoints::model_base_url()),
        super::request_headers::images(&token, &req.headers),
        body,
    ))
}
