//! Images 请求折成 Cloud Code Assist 的 image_gen 信封，响应保留 inlineData 给网关聚合。

use agent2api_provider_sdk::images::{ImagePlan, ImageRequest, ImageResponseKind};
use agent2api_provider_sdk::{control_error, PluginCtx};
use serde_json::{json, Value};

use super::{endpoints, models, oauth, project, PROVIDER_ID};

pub fn is_image_model(id: &str) -> bool {
    let lowered = id.to_ascii_lowercase();
    lowered.starts_with("gemini-") && lowered.contains("image")
}

fn image_config(body: &Value) -> Result<Value, String> {
    let size = body.get("size").and_then(Value::as_str).unwrap_or("auto");
    if size == "auto" {
        return Ok(if body.get("images").is_some() {
            json!({ "imageSize": "1K" })
        } else {
            json!({ "aspectRatio": "1:1", "imageSize": "1K" })
        });
    }
    let dimensions = size
        .split_once('x')
        .and_then(|(width, height)| Some((width.parse::<u32>().ok()?, height.parse::<u32>().ok()?)))
        .filter(|(width, height)| *width > 0 && *height > 0);
    let Some((width, height)) = dimensions else {
        return Err(control_error(
            400,
            "Antigravity size 必须是 auto 或 WIDTHxHEIGHT",
            None,
            None,
        ));
    };
    let ratio = width as f64 / height as f64;
    let ratios = [
        ("1:1", 1.0),
        ("2:3", 2.0 / 3.0),
        ("3:2", 1.5),
        ("3:4", 0.75),
        ("4:3", 4.0 / 3.0),
        ("4:5", 0.8),
        ("5:4", 1.25),
        ("9:16", 9.0 / 16.0),
        ("16:9", 16.0 / 9.0),
        ("21:9", 21.0 / 9.0),
    ];
    let aspect = ratios.iter()
        .min_by(|left, right| (ratio - left.1).abs().total_cmp(&(ratio - right.1).abs()))
        .filter(|(_, candidate)| (ratio - candidate).abs() / candidate <= 0.04)
        .map(|(name, _)| *name)
        .ok_or_else(|| control_error(400, "Antigravity 不支持该宽高比，请使用 1:1、2:3、3:2、3:4、4:3、4:5、5:4、9:16、16:9 或 21:9", None, None))?;
    // Gemini 按原生宽高比与 1K/2K/4K 档输出，不保证请求中的精确像素数。
    let edge = width.max(height);
    let resolution = match edge {
        1..=1792 => "1K",
        1793..=3072 => "2K",
        3073..=4096 => "4K",
        _ => {
            return Err(control_error(
                400,
                "Antigravity 图片尺寸最大为 4K 档",
                None,
                None,
            ))
        }
    };
    Ok(json!({ "aspectRatio": aspect, "imageSize": resolution }))
}

pub async fn build_edit(ctx: &PluginCtx, req: ImageRequest) -> Result<ImagePlan, String> {
    for field in ["mask", "input_fidelity"] {
        if req.body.get(field).is_some_and(|value| !value.is_null()) {
            return Err(control_error(
                400,
                format!("Antigravity 参考图编辑不支持 {field}"),
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
    let limit = if req.wire_model.starts_with("gemini-2.5-") {
        3
    } else {
        14
    };
    if images.len() > limit {
        return Err(control_error(
            400,
            format!("该 Antigravity 模型最多支持 {limit} 张参考图"),
            None,
            None,
        ));
    }
    if images.iter().any(|image| {
        !image
            .get("image_url")
            .and_then(Value::as_str)
            .is_some_and(|url| url.starts_with("data:image/"))
    }) {
        return Err(control_error(
            400,
            "Antigravity 图片编辑需要内联的参考图",
            None,
            None,
        ));
    }
    build(ctx, req).await
}

pub async fn build(ctx: &PluginCtx, req: ImageRequest) -> Result<ImagePlan, String> {
    let wire_model = models::upstream_model_id(&req.wire_model);
    if !is_image_model(&wire_model) {
        return Err(control_error(
            400,
            "Antigravity 生图请使用 gemini-*-image 模型",
            None,
            None,
        ));
    }
    if req.body.get("n").and_then(Value::as_u64).unwrap_or(1) != 1 {
        return Err(control_error(
            400,
            "Antigravity 单次生图仅支持 n=1",
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
                    format!("Antigravity 不支持 {field}={value}，分辨率请通过 size 指定"),
                    None,
                    None,
                ));
            }
        }
    }
    if req.body.get("style").is_some_and(|value| !value.is_null()) {
        return Err(control_error(
            400,
            "Antigravity 请在 prompt 中描述风格，不支持 style 参数",
            None,
            None,
        ));
    }
    let image_config = image_config(&req.body)?;
    let mut credentials = oauth::ensure_fresh(ctx, &req.account, false)
        .await
        .map_err(crate::errors::GatewayError::into_control_message)?;
    if credentials.access_token.trim().is_empty() {
        credentials = oauth::ensure_fresh(ctx, &req.account, true)
            .await
            .map_err(crate::errors::GatewayError::into_control_message)?;
    }
    if credentials.project_id.trim().is_empty() {
        credentials.project_id =
            project::discover_project(&credentials.access_token, req.proxy.as_ref())
                .await
                .map_err(crate::errors::GatewayError::into_control_message)?;
        if let Ok(record) = crate::store::record(ctx, PROVIDER_ID, &req.account).await {
            let expect = crate::store::secret_view(&record);
            let next =
                crate::store::with_fields(&expect, &[("projectId", json!(credentials.project_id))]);
            let account = record
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(&req.account);
            let _ = crate::store::save(ctx, account, &expect, &next).await;
        }
    }
    let mut content = vec![json!({ "type": "text", "text": req.body.get("prompt") })];
    if let Some(images) = req.body.get("images").and_then(Value::as_array) {
        content.extend(images.iter().map(|image| {
            json!({
                "type": "image_url", "image_url": { "url": image.get("image_url") },
            })
        }));
    }
    let chat = json!({
        "model": wire_model,
        "messages": [{ "role": "user", "content": content }],
    });
    let email = credentials.email.trim();
    let enterprise =
        !email.is_empty() && !email.ends_with("@gmail.com") && !email.ends_with("@googlemail.com");
    let mut envelope = super::antigravity_outbound::antigravity_request_from_chat(
        &chat,
        &wire_model,
        &credentials.project_id,
        enterprise,
    )
    .map_err(|error| {
        control_error(
            400,
            format!("Antigravity 生图请求转换失败：{error}"),
            None,
            None,
        )
    })?;
    envelope["request"]["generationConfig"]["responseModalities"] = json!(["TEXT", "IMAGE"]);
    envelope["request"]["generationConfig"]["imageConfig"] = image_config;
    Ok(ImagePlan {
        url: endpoints::stream_generate_url(endpoints::V1_BASE_URL_SANDBOX),
        headers: endpoints::content_headers(&credentials.access_token),
        body: envelope,
        response: ImageResponseKind::Gemini,
    })
}
