//! WorkBuddy 文生图 / 参考图编辑：官方客户端 `ImageService` 的同一对路由。
//!
//! ── 契约从哪来 ──────────────────────────────────────────────
//! 从官方客户端（`WorkBuddyAI/resources/app.asar.unpacked/cli/dist/codebuddy.js`
//! 的 `ImageService`）读出来的，逐字对齐：
//!
//! ```text
//!   文生图   POST {endpoint}/v2/images/generations   （Bearer accessToken，纯 JSON）
//!   参考图   POST {endpoint}/v2/images/edits
//! ```
//!
//! `endpoint` 就是 `/v3/config` 的 `endpoint`（国内版实测 `https://copilot.tencent.com`），
//! 与对话共用同一个 `session_endpoint_of`。**请求体不是 multipart** —— 参考图编码成
//! `data:` URL 放进 `image` 数组，这一点与 Raccoon 的 multipart 通道完全不同。
//!
//! 请求体按模型名前缀分三档（官方 `buildBaseRequestBody` 的原样分支）：
//!
//! | 分支 | 认的字段 |
//! |---|---|
//! | `hunyuan-*` | `model` / `prompt` / `size` / `n` / `footnote` / `revise` |
//! | `gemini-*` | `model` / `prompt` / `size` |
//! | 其余 | 上面三项 + `response_format: "b64_json"` / `n` / `quality` / `style` / `background` |
//!
//! 响应统一是 `{code, msg, request_id, data:{data:[…], usage:{credit}}}` 一层信封，
//! 由网关的 `ImageResponseKind::Workbuddy` 拆开（见 `core::upstream::images`）。
//!
//! ── 模型是怎么被登记的 ──────────────────────────────────────
//! 图像条目不靠 id 白名单，靠 `/v3/config` 的 `tags`：`text-to-image` 是文生图、
//! `image-to-image` 是参考图编辑。理由与官方客户端一致 —— 上游换 id（内置兜底
//! 是另一对 `hunyuan-image-v3.0` / `hunyuan-image-v2.0-general-edit`，实测线上派发
//! 的又是 `hunyuan-image-alpha*`）比换标签频繁得多。所以目录刷新时按标签把它们
//! 摘出来（`pick`），能力位与 `kind` 都在这里补上。
//!
//! 两个地区共用这套实现，但**哪条模型可用由各自账号的 `/v3/config` 说了算**：
//! 国内版实测会下发 `hunyuan-image-alpha*`（本文件的内置兜底即取自它）；国际版
//! 的目录里一个腾讯模型都没有，它有没有图像条目尚未实测 —— 没有就自然一条都
//! 不暴露，不会凭代码凭空造出一个上游不认的模型名。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use agent2api_provider_sdk::images::{ImagePlan, ImageRequest, ImageResponseKind};
use agent2api_provider_sdk::{control_error, PluginCtx};
use serde_json::{json, Value};

use super::region::Region;

/// 上游给图像条目的分类标签（`/v3/config` 的 `models[].tags`）。
const TEXT_TO_IMAGE_TAG: &str = "text-to-image";
const IMAGE_TO_IMAGE_TAG: &str = "image-to-image";

/// 客户端未指定 `size` 时上游的默认值（官方客户端硬编码同一个）。
const DEFAULT_SIZE: &str = "1024x1024";

/// 参考图张数上限（与网关 `api::image_input` 的 16 张口径一致）。
const MAX_IMAGE_REFERENCES: usize = 16;

/// 这条条目是不是图像模型；是的话给出它支持哪几类操作。
///
/// 判据只有标签，不看 id —— 见模块头。两条标签都没有就返回 `None`（对话模型）。
pub fn capabilities(row: &Value) -> Option<(bool, bool)> {
    let tags = row.get("tags").and_then(Value::as_array)?;
    let has = |tag: &str| tags.iter().any(|value| value.as_str() == Some(tag));
    let generation = has(TEXT_TO_IMAGE_TAG);
    let editing = has(IMAGE_TO_IMAGE_TAG);
    (generation || editing).then_some((generation, editing))
}

pub fn is_image(row: &Value) -> bool {
    capabilities(row).is_some()
}

/// 把一条上游图像条目补成下游认的形态：`kind=image` 让它进图像通道，
/// `supports_image_generation` / `supports_image_editing` 决定它出现在
/// `/v1/images/generations` 还是 `/v1/images/edits` 的可用模型里。
///
/// 与 ModelScope 的同名函数口径一致：`supportsImages`（多模态**输入**）只对
/// 参考图编辑那一类开 —— 文生图模型不吃图，标成能吃会让客户端往上送图。
pub fn decorate(row: &Value) -> Option<Value> {
    let (generation, editing) = capabilities(row)?;
    let mut item = row.clone();
    item["kind"] = json!("image");
    item["output_modalities"] = json!(["image"]);
    item["supports_image_generation"] = json!(generation);
    item["supports_image_editing"] = json!(editing);
    item["supportsToolCall"] = json!(false);
    item["supportsImages"] = json!(editing);
    item["supportsReasoning"] = json!(false);
    Some(item)
}

/// 从一次 `/v3/config` 的条目里摘出全部图像模型（目录刷新用）。
pub fn pick(rows: &[Value]) -> Vec<Value> {
    rows.iter().filter_map(decorate).collect()
}

/// 内置兜底：`/v3/config` 一次都没拉成功时，`/v1/models` 里的两条图像条目。
///
/// id 取**实测**值（2026-11 的 `/v3/config` 派发 `hunyuan-image-alpha` /
/// `hunyuan-image-alpha-edit`）。它只在「还没有远程目录」的窗口里顶班，与对话
/// 模型的内置清单同一个道理；远程目录一到就整份替换掉它。
pub fn builtin() -> Vec<Value> {
    [
        ("hunyuan-image-alpha", "Hunyuan Image Alpha", TEXT_TO_IMAGE_TAG),
        (
            "hunyuan-image-alpha-edit",
            "Hunyuan Image Alpha Edit",
            IMAGE_TO_IMAGE_TAG,
        ),
    ]
    .into_iter()
    .filter_map(|(id, name, tag)| {
        decorate(&json!({ "id": id, "name": name, "tags": [tag] }))
    })
    .collect()
}

/// 某条 wire model 支持哪几类操作：先查目录（带标签的真身），查不到按 id 兜底。
///
/// 兜底那一支只服务「目录里没有这条模型」的边角情形（比如用户手填了一个 id）：
/// 网关正常只会把目录里声明过的模型路由到这里，所以它不是主路径。
fn operations(region: Region, model: &str) -> (bool, bool) {
    if let Some(row) = super::catalog_state::model(region, model) {
        if let Some(capabilities) = capabilities(&row) {
            return capabilities;
        }
    }
    let editing = model.to_ascii_lowercase().contains("edit");
    (!editing, editing)
}

pub async fn build(
    ctx: &PluginCtx,
    req: ImageRequest,
    region: Region,
    editing: bool,
) -> Result<ImagePlan, String> {
    let (generation, editable) = operations(region, &req.wire_model);
    if editing && !editable {
        return Err(control_error(
            400,
            "WorkBuddy 该模型不支持参考图编辑，请选择带 -edit 的图片模型",
            None,
            None,
        ));
    }
    if !editing && !generation {
        return Err(control_error(
            400,
            "WorkBuddy 该模型不支持文生图，请选择图片生成模型",
            None,
            None,
        ));
    }
    let prompt = req
        .body
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .ok_or_else(|| control_error(400, "图片请求缺少 prompt", None, None))?;
    // `auto` 是网关侧的「让上游自己定」：官方客户端从不发这个值，所以按缺省处理
    // （上游在缺 `size` 时自己回落到 1024x1024）。
    let size = req
        .body
        .get("size")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|size| !size.is_empty() && !size.eq_ignore_ascii_case("auto"))
        .unwrap_or(DEFAULT_SIZE);
    let mut body = json!({ "model": req.wire_model, "prompt": prompt, "size": size });
    let hunyuan = req.wire_model.starts_with("hunyuan-");
    let gemini = req.wire_model.starts_with("gemini-");
    // 上游的原生图片通道（hunyuan / gemini）只认 `model` / `prompt` / `size`
    // （hunyuan 另认 `n`），与 OpenAI 兼容那一档不是同一套字段。多出来的字段
    // **明确拒绝**而不是静默丢掉：静默会让用户以为风格/质量生效了，而上游根本
    // 没收到 —— 这类「看起来成功了但没生效」最难查。
    if hunyuan || gemini {
        if let Some(field) = ["quality", "style", "background", "moderation"]
            .into_iter()
            .find(|field| req.body.get(*field).is_some_and(|value| !value.is_null()))
        {
            return Err(control_error(
                400,
                format!("WorkBuddy 该图片模型不支持 {field}，请把要求写进 prompt"),
                None,
                None,
            ));
        }
    }
    if hunyuan {
        body["n"] = json!(req.body.get("n").and_then(Value::as_u64).unwrap_or(1));
    } else if !gemini {
        body["response_format"] = json!("b64_json");
        if let Some(n) = req.body.get("n").and_then(Value::as_u64) {
            body["n"] = json!(n);
        }
        for key in ["quality", "style", "background"] {
            if let Some(value) = req.body.get(key).filter(|value| !value.is_null()) {
                body[key] = value.clone();
            }
        }
    }
    if editing {
        let images = req
            .body
            .get("images")
            .and_then(Value::as_array)
            .filter(|images| !images.is_empty() && images.len() <= MAX_IMAGE_REFERENCES)
            .ok_or_else(|| {
                control_error(
                    400,
                    format!("WorkBuddy 参考图编辑需要 1 至 {MAX_IMAGE_REFERENCES} 张图片"),
                    None,
                    None,
                )
            })?;
        body["image"] = Value::Array(
            images
                .iter()
                .map(|image| image.get("image_url").cloned().unwrap_or(Value::Null))
                .collect(),
        );
        // 上游认这个字段（官方客户端只在传了才带）。网关当前在入口就拒掉它，
        // 所以这里通常拿不到值 —— 留着这一手是为了网关放开后不用再改插件。
        if let Some(fidelity) = req.body.get("input_fidelity").filter(|value| !value.is_null()) {
            body["input_fidelity"] = fidelity.clone();
        }
    }
    let session = super::session_for(ctx, region, &req.account)
        .await
        .map_err(|error| error.into_control_message())?;
    let path = if editing { "edits" } else { "generations" };
    let mut headers = super::contract::framework_headers(&session, region, true);
    headers.retain(|(name, _)| !name.eq_ignore_ascii_case("content-type"));
    headers.push(("Content-Type".to_string(), "application/json".to_string()));
    Ok(ImagePlan {
        url: format!("{}/v2/images/{path}", super::session_endpoint_of(&session, region)),
        headers,
        body,
        response: ImageResponseKind::Workbuddy,
    })
}
