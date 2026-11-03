//! 可选 Images 扩展；不改变既有握手或 Chat 协议，旧插件返回 NO_METHOD。

use std::collections::BTreeMap;

use agent2api_provider_protocol::WireProxy;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const CAPABILITIES_METHOD: &str = "images.capabilities";
pub const GENERATE_METHOD: &str = "images.generate";
pub const EDIT_METHOD: &str = "images.edit";
pub const TRANSPORT_CAPABILITIES_METHOD: &str = "transport.capabilities";

impl crate::PluginCtx {
    /// 旧宿主没有该回调时不能发送 multipart 计划，避免把上传信封误当 JSON。
    pub async fn host_supports_multipart(&self) -> bool {
        self.call
            .call(TRANSPORT_CAPABILITIES_METHOD, serde_json::json!({}))
            .await
            .ok()
            .and_then(|value| value.get("multipart").and_then(Value::as_bool))
            .unwrap_or(false)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MultipartFile {
    pub field: String,
    pub filename: String,
    pub content_type: String,
    pub base64: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImageRequest {
    pub provider: String,
    pub account: String,
    #[serde(default)]
    pub proxy: Option<WireProxy>,
    pub body: Value,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub wire_model: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageResponseKind {
    Openai,
    Gemini,
    ModelscopeAsync,
    /// WorkBuddy：`{code, msg, data:{data:[…], usage:{credit}}}` 一层信封。
    Workbuddy,
    /// 豆包工作：出图是**对话流**里的一次 agent 任务，图片挂在 `gen_image_block`
    /// （SSE）；带水印的预览与**无水印原图**都在流里（后者在
    /// `rich_media_layout_block` 的 `image_ori_raw`），流里没给才拿 `uri` 再向
    /// `/creativity/resource/get_without_watermark` 换一次。取图这一步在宿主侧完成
    /// （插件只给第一步的计划），理由见网关 `core/upstream/images.rs` 的 `doubao_body`。
    Doubao,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImagePlan {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
    pub response: ImageResponseKind,
}

impl ImagePlan {
    pub fn openai(url: String, headers: Vec<(String, String)>, body: Value) -> Self {
        Self {
            url,
            headers,
            body,
            response: ImageResponseKind::Openai,
        }
    }

    pub fn openai_multipart(
        url: String,
        mut headers: Vec<(String, String)>,
        fields: Value,
        files: Vec<MultipartFile>,
    ) -> Self {
        headers.retain(|(name, _)| !name.eq_ignore_ascii_case("content-type"));
        headers.push((
            "Content-Type".to_string(),
            "multipart/form-data".to_string(),
        ));
        Self::openai(
            url,
            headers,
            serde_json::json!({
                "multipart": { "fields": fields, "files": files },
            }),
        )
    }
}
