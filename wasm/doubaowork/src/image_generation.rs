use std::collections::BTreeSet;

use agent2api_wasm_sdk::{self as sdk, images::Resource, json, Error, Result, Value};

use crate::{account, catalog, chat, endpoints::ReadEndpoint, errors, prompt, stream::Decoder};

pub const MODEL: &str = "doubao-image";
pub fn model_row() -> Value {
    json!({"id":MODEL,"name":"豆包生图","kind":"image","output_modalities":["image"],"supports_image_generation":true,
        "supports_image_editing":false,"supportsImages":false,"supportsToolCall":false,"supportsReasoning":false})
}

pub fn generate(params: &Value) -> Result<Value> {
    let body = &params["body"];
    if body["model"] != MODEL {
        return Err(unsupported("请选择豆包生图模型"));
    }
    if [
        "images",
        "image",
        "mask",
        "input_fidelity",
        "quality",
        "style",
        "background",
        "moderation",
    ]
    .iter()
    .any(|field| body.get(*field).is_some_and(|value| !value.is_null()))
    {
        return Err(unsupported(
            "豆包工作生图不支持参考图、蒙版或专用风格参数，请把画面要求写进提示词",
        ));
    }
    if body.get("n").is_some_and(|value| value.as_u64() != Some(1)) {
        return Err(unsupported("豆包工作一次任务只生成一张图片"));
    }
    let prompt = body["prompt"]
        .as_str()
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty() && prompt.len() <= 256 * 1024)
        .ok_or_else(|| unsupported("请输入图片描述"))?;
    let ratio = match body.get("size") {
        None | Some(Value::Null) => None,
        Some(Value::String(size)) if size == "auto" => None,
        Some(Value::String(size)) => Some(
            ratio(size)
                .ok_or_else(|| unsupported("豆包工作无法可靠表达此图片尺寸，请使用常见画面比例"))?,
        ),
        _ => return Err(unsupported("图片尺寸必须是文本")),
    };
    let text = if let Some(ratio) = ratio {
        format!(
            "请直接用图片生成能力画出来，不要只给文字描述。画面比例 {ratio}，画面内容：{prompt}"
        )
    } else {
        format!("请直接用图片生成能力画出来，不要只给文字描述。画面内容：{prompt}")
    };
    let current = account::current()?;
    current.verify()?;
    let target = catalog::agent_target()?;
    let credentials = current
        .credentials
        .with_runtime_device(target.device_id.as_deref());
    credentials.chat_context()?;
    let payload = prompt::payload(
        &target.bot,
        &target.model,
        &text,
        Vec::new(),
        &json!({}),
        true,
    )?;
    let mut decoder = Decoder::default();
    chat::consume(
        &current,
        &credentials,
        ReadEndpoint::Chat,
        &payload,
        &mut decoder,
        None,
        false,
    )?;
    let mut consumed = BTreeSet::new();
    loop {
        let receipt = sdk::host("doubao.turn.receipt", json!({}))?;
        let task = receipt["tasks"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|task| {
                task["completed"] != true
                    && task["task_id"]
                        .as_str()
                        .is_some_and(|id| !consumed.contains(id))
            })
            .cloned();
        let Some(task) = task else {
            break;
        };
        let id = task["task_id"]
            .as_str()
            .ok_or_else(|| errors::protocol("豆包工作任务标识无效"))?
            .to_string();
        if consumed.len() >= 100 || !consumed.insert(id.clone()) {
            return Err(errors::protocol("豆包工作任务数量超过限制"));
        }
        chat::consume(
            &current,
            &credentials,
            ReadEndpoint::AsyncChunk,
            &json!({"task_id":id,"seq_start":0,"append_scene":task["append_scene"],"ext":{}}),
            &mut decoder,
            None,
            false,
        )?;
    }
    if let Some(error) = decoder.restriction().cloned() {
        sdk::host("doubao.turn.finish", json!({}))?;
        return Err(error);
    }
    let urls = decoder.image_urls();
    if urls.is_empty() {
        return Err(
            Error::http(502, "豆包工作任务已结束，但没有交付图片；未自动重新生成")
                .with_code("image_output_uncertain"),
        );
    }
    let mut images = Vec::new();
    for url in urls {
        current.unchanged()?;
        let resource = Resource::fetch_public(&url)?;
        current.unchanged()?;
        images.push(resource.image_result()?);
    }
    current.unchanged()?;
    sdk::host("doubao.turn.finish", json!({}))?;
    Ok(json!({"created":sdk::now()/1000,"data":images}))
}
fn ratio(size: &str) -> Option<&'static str> {
    let (width, height) = size.split_once('x').or_else(|| size.split_once('X'))?;
    let width: f64 = width.parse().ok()?;
    let height: f64 = height.parse().ok()?;
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return None;
    }
    let target = width / height;
    [
        (1.0, "1:1"),
        (16.0 / 9.0, "16:9"),
        (9.0 / 16.0, "9:16"),
        (4.0 / 3.0, "4:3"),
        (3.0 / 4.0, "3:4"),
        (3.0 / 2.0, "3:2"),
        (2.0 / 3.0, "2:3"),
        (21.0 / 9.0, "21:9"),
    ]
    .into_iter()
    .min_by(|left, right| (left.0 - target).abs().total_cmp(&(right.0 - target).abs()))
    .filter(|(ratio, _)| (ratio - target).abs() / target <= 0.08)
    .map(|(_, name)| name)
}
fn unsupported(message: &str) -> Error {
    Error::http(400, message).with_code("unsupported_parameter")
}
