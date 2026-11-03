//! 豆包工作文生图：在对话里让 agent 跑一次 `image_gen` 任务，再换无水印原图。
//!
//! ── 上游没有独立的生图接口 ──────────────────────────────────
//! 实测（2026-11-03，本机客户端抓包 + 复现）之后确认：豆包工作**没有**那种
//! 「提交一次请求、返回图片地址」的图片接口，出图就是「在对话里派一次任务」——
//! agent 自己调用 `image_gen` 技能，结果作为 `gen_image_block`
//! （`block_type = 10010`）挂在 `/chat/completion` 那条 SSE 流里。所以本模块
//! 交出去的 `ImagePlan` 本身就是一次对话请求；响应类型标成
//! `ImageResponseKind::Doubao` 由宿主侧认领 —— 从流里取图、换无水印原图这两步
//! 在 `core::upstream::images` 里（那边有为什么必须放在宿主侧的说明：`ImagePlan`
//! 只表达"一次请求"，换原图是第二次请求）。
//!
//! ── 用哪条模型跑这次任务 ────────────────────────────────────
//! 生图能力挂在**通用任务模式**上，与目录里的对话模型不是一回事：客户端那份
//! 会话记录里 `ext.agent_name = Agent-GeneralTask`、`ext.agent_mode = 1`。
//! 所以这里不解析下游传的模型名，而是按 `catalog::resolve_agent` 挑一条能跑
//! 任务的模型（`agent_mode != 0`）。下游看到的 id 是 [`MODEL_ID`]，对应目录里
//! 那条 `kind = image` 的条目（见 `catalog::model_records` / [`model_row`]）——
//! 能力位与 id 两边必须一致，否则网关不会把生图请求派到本家（`operation_providers`
//! 是拿目录条目判的）。
//!
//! ── 参数口径 ────────────────────────────────────────────────
//! `size` 翻成画面比例提示写进 prompt：豆包是从自然语言里读比例的（实测把
//! "9:16" 写进 prompt 就会出竖版，画面比例也确实是 9:16）。`quality` / `style` /
//! `background` / `moderation` 这些 OpenAI 专属字段上游没有对应物，**明确拒绝**
//! 而不是静默丢掉 —— 静默会让用户以为风格生效了，这类「看起来成功了但没生效」
//! 最难查（与 WorkBuddy / ModelScope 两家的口径一致）。`n` 只支持 1：一次任务
//! 出一张，要两张得让 agent 多轮。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use agent2api_provider_sdk::images::{ImagePlan, ImageRequest, ImageResponseKind};
use agent2api_provider_sdk::{control_error, PluginCtx};
use serde_json::{json, Value};

use super::{api, catalog, prompt};

/// 下游认的模型 id（目录里那条图像条目的 id）。
///
/// 不借用上游真身（`seedream_5_0_flash` 之类）而另起一个名字，是因为**我们不是
/// 直连那个图像模型**：这条通道发出去的是对话请求，图片由 agent 的任务产出。
/// 用上游真身当 id 会让人以为"请求 seedream 就等价于请求上游 seedream"。
/// 展示名与 id 都写在 [`model_row`] 里，两边一起改。
pub const MODEL_ID: &str = "doubao-image";

/// 目录里那条图像条目：`kind = image` 让它进图像通道，
/// `supports_image_generation` 决定它出现在 `/v1/images/generations` 的可用模型里。
///
/// 与 ModelScope / WorkBuddy 的同名函数口径一致：`supportsImages`（多模态**输入**）
/// 只对参考图那一类开 —— 本家不吃参考图，标成能吃会让客户端往上送图。
pub fn model_row() -> Value {
    json!({
        "id": MODEL_ID,
        "name": "豆包生图",
        "kind": "image",
        "output_modalities": ["image"],
        "supports_image_generation": true,
        "supports_image_editing": false,
        "supportsToolCall": false,
        "supportsImages": false,
        "supportsReasoning": false,
    })
}

/// 下游的 `size` 翻成豆包认的画面比例提示。
///
/// 只认常见比例，且**差得太远就不猜**（宁可不说，也不要给一个错的画面比例）——
/// 上游是从自然语言里读的，说错了它会照着错的画。
fn ratio(size: &str) -> Option<&'static str> {
    if size.eq_ignore_ascii_case("auto") {
        return None;
    }
    let (width, height) = size
        .trim()
        .split_once('x')
        .or_else(|| size.trim().split_once('X'))
        .map(|(width, height)| (width.trim(), height.trim()))?;
    let width: f64 = width.parse().ok()?;
    let height: f64 = height.parse().ok()?;
    if !(width.is_finite() && height.is_finite()) || width <= 0.0 || height <= 0.0 {
        return None;
    }
    let target = width / height;
    // 客户端常见取值的比例：1024x1024(1:1)、1792x1024(16:9)、1024x1792(9:16) 等
    let table: [(f64, &'static str); 8] = [
        (1.0, "1:1"),
        (16.0 / 9.0, "16:9"),
        (9.0 / 16.0, "9:16"),
        (4.0 / 3.0, "4:3"),
        (3.0 / 4.0, "3:4"),
        (3.0 / 2.0, "3:2"),
        (2.0 / 3.0, "2:3"),
        (21.0 / 9.0, "21:9"),
    ];
    let (matched, name) = table
        .into_iter()
        .min_by(|left, right| (left.0 - target).abs().total_cmp(&(right.0 - target).abs()))?;
    ((matched - target).abs() / target <= 0.08).then_some(name)
}

/// 这次任务给 agent 的话：把「出图」这个要求说出来，并把画面比例一并交代。
///
/// 客户端自己也是这么干的（用户在输入框里写"帮我生成一张…9:16"），实测能稳定
/// 触发 `image_gen`。补一句"不要只给文字描述"是因为 agent 偶尔会先回一段画法
/// 说明；加上它出图成功率明显更高。
fn instruction(prompt: &str, ratio: Option<&str>) -> String {
    match ratio {
        Some(ratio) => format!(
            "请直接用图片生成能力画出来，不要只给文字描述。画面比例 {ratio}，画面内容：{prompt}"
        ),
        None => format!("请直接用图片生成能力画出来，不要只给文字描述。画面内容：{prompt}"),
    }
}

pub async fn build(ctx: &PluginCtx, req: ImageRequest) -> Result<ImagePlan, String> {
    // 参考图：本家不吃图（`supports_image_editing = false`，网关不会把编辑请求
    // 派过来）。真收到了就说清楚，而不是拼一个没有图片的请求发上去。
    if ["images", "image", "mask", "input_fidelity"]
        .iter()
        .any(|field| req.body.get(*field).is_some_and(|value| !value.is_null()))
    {
        return Err(control_error(
            400,
            "豆包工作生图不支持参考图，请改用支持图片编辑的模型",
            None,
            None,
        ));
    }
    if let Some(field) = ["quality", "style", "background", "moderation"]
        .into_iter()
        .find(|field| req.body.get(*field).is_some_and(|value| !value.is_null()))
    {
        return Err(control_error(
            400,
            format!("豆包工作生图不支持 {field}，请把要求写进 prompt"),
            None,
            None,
        ));
    }
    if req.body.get("n").and_then(Value::as_u64).unwrap_or(1) != 1 {
        return Err(control_error(
            400,
            "豆包工作一次任务只出一张图，请把 n 设为 1（需要多张请让对话再生成一次）",
            None,
            None,
        ));
    }
    let prompt_text = req
        .body
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .ok_or_else(|| control_error(400, "图片请求缺少 prompt", None, None))?;
    let hint = req.body.get("size").and_then(Value::as_str).and_then(ratio);
    let text = instruction(prompt_text, hint);
    // `false` = 控制面客户端（带总超时）：这里只用它拉目录与控制面，
    // 真正那次对话请求交给宿主发（见模块头），不必用流式客户端。
    let (account, credentials, client) = api::account(ctx, &req.account, false)
        .await
        .map_err(|error| error.into_control_message())?;
    let (bot, model) = catalog::resolve_agent(ctx, &account, &credentials, &client)
        .await
        .map_err(|error| error.into_control_message())?;
    let body = prompt::payload(&bot, &model, &text, Vec::new(), &json!({}))
        .map_err(|error| error.into_control_message())?;
    let mut headers = api::headers(&credentials).map_err(|error| error.into_control_message())?;
    headers.push(("Content-Type".into(), "application/json".into()));
    Ok(ImagePlan {
        url: api::endpoint(&credentials, "/chat/completion")
            .map_err(|error| error.into_control_message())?,
        headers,
        body,
        response: ImageResponseKind::Doubao,
    })
}
