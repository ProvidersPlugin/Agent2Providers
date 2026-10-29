//! API-Inference 文档确认 /v1/images/generations + image_url 数组 + /v1/tasks/{id}。
//! https://modelscope.ai/docs/model-service/API-Inference/intro
//! 仅登记文档和官方模型卡确认的型号，不把整个模型市场当作账号可调用清单。

use agent2api_provider_sdk::{control_error, PluginCtx};
use agent2api_provider_sdk::images::{ImagePlan, ImageRequest, ImageResponseKind};
use serde_json::{json, Value};
use super::{credentials, Region};

pub fn capabilities(id: &str) -> Option<(bool, bool)> {
    let generation = ["Qwen/Qwen-Image", "Qwen/Qwen-Image-2512", "Tongyi-MAI/Z-Image-Turbo", "Tongyi-MAI/Z-Image"];
    let editing = ["Qwen/Qwen-Image-Edit", "Qwen/Qwen-Image-Edit-2509", "Qwen/Qwen-Image-Edit-2511"];
    if generation.iter().any(|model| model.eq_ignore_ascii_case(id)) { Some((true, false)) }
    else if editing.iter().any(|model| model.eq_ignore_ascii_case(id)) { Some((false, true)) }
    else { None }
}

pub fn decorate(id: &str, row: &mut Value) {
    if let Some((generation, editing)) = capabilities(id) {
        row["kind"] = json!("image");
        row["output_modalities"] = json!(["image"]);
        row["supports_image_generation"] = json!(generation);
        row["supports_image_editing"] = json!(editing);
        row["supportsToolCall"] = json!(false);
        row["supportsImages"] = json!(editing);
        row["supportsReasoning"] = json!(false);
    }
}

pub async fn build(ctx: &PluginCtx, req: ImageRequest, region: Region, editing: bool) -> Result<ImagePlan, String> {
    let cap = capabilities(&req.wire_model).ok_or_else(|| control_error(400, "该 ModelScope 型号尚未确认支持 Images 接口", None, None))?;
    if (editing && !cap.1) || (!editing && !cap.0) { return Err(control_error(400, "该 ModelScope 型号不支持这类图片操作，请选择对应的生图或编辑模型", None, None)); }
    if req.body.get("n").and_then(Value::as_u64).unwrap_or(1) != 1 { return Err(control_error(400, "ModelScope 异步图片接口单次仅支持 n=1", None, None)); }
    for (field, allowed) in [("quality", &["auto", "standard"][..]), ("background", &["auto", "opaque"][..]), ("moderation", &["auto"][..])] {
        if let Some(value) = req.body.get(field).filter(|value| !value.is_null()) {
            if !value.as_str().is_some_and(|value| allowed.contains(&value)) { return Err(control_error(400, format!("ModelScope 不支持 {field}={value}"), None, None)); }
        }
    }
    for field in ["style", "mask", "input_fidelity"] {
        if req.body.get(field).is_some_and(|value| !value.is_null()) { return Err(control_error(400, format!("ModelScope 图片接口暂不支持 {field}"), None, None)); }
    }
    let mut body = json!({ "model": req.wire_model, "prompt": req.body.get("prompt") });
    if let Some(size) = req.body.get("size").and_then(Value::as_str).filter(|value| *value != "auto") {
        let (width, height) = size.split_once('x').and_then(|(width, height)| Some((width.parse::<u32>().ok()?, height.parse::<u32>().ok()?)))
            .filter(|(width, height)| (64..=2048).contains(width) && (64..=2048).contains(height)).ok_or_else(|| control_error(400, "ModelScope size 需要 64 至 2048 范围内的 WIDTHxHEIGHT", None, None))?;
        body["size"] = json!(format!("{width}x{height}"));
    }
    if editing {
        let images = req.body.get("images").and_then(Value::as_array).filter(|items| !items.is_empty() && items.len() <= 3)
            .ok_or_else(|| control_error(400, "ModelScope 参考图编辑需要 1 至 3 张图片", None, None))?;
        body["image_url"] = Value::Array(images.iter().map(|image| image.get("image_url").cloned().unwrap_or(Value::Null)).collect());
    }
    let account = ctx.store_get(region.provider_id(), &req.account).await.map_err(|error| format!("读取 ModelScope 账号失败：{error}"))?;
    let token = credentials::token_of(&account.secret)?;
    Ok(ImagePlan { url: format!("{}/images/generations", region.inference_base_url()), body,
        headers: vec![("Content-Type".into(), "application/json".into()), ("Authorization".into(), format!("Bearer {token}")), ("X-ModelScope-Async-Mode".into(), "true".into())],
        response: ImageResponseKind::ModelscopeAsync })
}
