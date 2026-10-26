//! 官方 imageGeneration 使用 /images/gen，目录未提供模型 ID；raccoon-image 是网关别名。

use agent2api_provider_sdk::images::{ImagePlan, ImageRequest};
use agent2api_provider_sdk::{control_error, PluginCtx, Provider};
use serde_json::{json, Value};

pub const MODEL_ID: &str = "raccoon-image";

pub fn model() -> Value {
    json!({
        "id": MODEL_ID,
        "name": "Raccoon Image",
        "kind": "image",
        "synthetic": true,
        "output_modalities": ["image"],
        "supportsImages": false,
        "supportsToolCall": false,
        "supportsReasoning": false,
        "isDefault": false,
    })
}

pub async fn build(ctx: &PluginCtx, req: ImageRequest) -> Result<ImagePlan, String> {
    if !req.wire_model.eq_ignore_ascii_case(MODEL_ID) {
        return Err(control_error(
            400,
            "小浣熊生图请使用 raccoon-image",
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
                    format!("小浣熊生图不支持参数 {field}={value}"),
                    None,
                    None,
                ));
            }
        }
    }
    if req.body.get("style").is_some_and(|value| !value.is_null()) {
        return Err(control_error(
            400,
            "小浣熊生图请在 prompt 中描述风格，不支持 style 参数",
            None,
            None,
        ));
    }
    let token = super::RaccoonProvider
        .ensure_token(ctx, &req.account)
        .await?;
    if token.trim().is_empty() {
        return Err(control_error(
            401,
            "小浣熊账号缺少 accessToken，无法生图",
            None,
            None,
        ));
    }
    let mut body = json!({ "prompt": req.body.get("prompt"), "n": req.body.get("n").cloned().unwrap_or(json!(1)) });
    if let Some(size) = req
        .body
        .get("size")
        .filter(|value| value.as_str() != Some("auto") && !value.is_null())
    {
        body["size"] = size.clone();
    }
    Ok(ImagePlan::openai(
        format!("{}/images/gen", super::llm_base_url()),
        vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "application/json".to_string()),
            ("Authorization".to_string(), format!("Bearer {token}")),
        ],
        body,
    ))
}
