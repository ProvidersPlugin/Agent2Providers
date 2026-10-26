//! Loomy 模型网关的 Images API，使用与模型目录相同的 token + Bearer 双头。

use agent2api_provider_sdk::images::{ImagePlan, ImageRequest};
use agent2api_provider_sdk::{control_error, PluginCtx};
use serde_json::Value;

pub async fn build_edit(ctx: &PluginCtx, mut req: ImageRequest) -> Result<ImagePlan, String> {
    // 官方客户端的 _generateImageToImage 仍调用 generations，images 是 data URL 数组。
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
                .filter(|url| url.starts_with("data:image/"))
                .map(|url| Value::String(url.to_string()))
                .ok_or_else(|| control_error(400, "Loomy 图片编辑需要内联的参考图", None, None))
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
    let credentials = super::credentials::snapshot(ctx, &req.account).await?;
    let token = credentials.session;
    let mut body = req.body;
    if let Some(object) = body.as_object_mut() {
        // 响应格式和文件编码由网关处理；上游默认返回签名 URL。
        for key in [
            "response_format",
            "output_format",
            "output_compression",
            "stream",
            "partial_images",
            "mask",
            "input_fidelity",
        ] {
            object.remove(key);
        }
        if object
            .get("size")
            .is_some_and(|value| value.as_str() == Some("auto") || value.is_null())
        {
            object.remove("size");
        }
    }
    Ok(ImagePlan::openai(
        format!("{}/images/generations", super::endpoints::model_base_url()),
        vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "application/json".to_string()),
            ("token".to_string(), token.clone()),
            ("Authorization".to_string(), format!("Bearer {token}")),
        ],
        body,
    ))
}
