//! 生图提交可能扣费，仅发一次；任务轮询失败保留ID，不换账号或重新生成。
use agent2api_wasm_sdk::{self as sdk, images::Resource, json, Error, Result, Value};

use crate::{account, endpoints, token, util};

const MODELS: &[(&str, &str)] = &[
    ("gemini-nano-banana-2.1", "Nano Banana 2.1"),
    ("gpt-image-2.5", "GPT Image 2.5"),
    ("gpt-image-2", "GPT Image 2"),
    ("gemini-3-pro-image", "Gemini 3 Pro Image"),
    ("gemini-3-pro-image-preview", "Gemini 3 Pro Image Preview"),
];
pub fn is_model(id: &str) -> bool {
    MODELS.iter().any(|(model, _)| *model == id)
}
pub fn models() -> Vec<Value> {
    MODELS.iter().map(|(id, name)| json!({ "id": id, "name": name, "kind": "image", "type": "image",
        "output_modalities": ["image"], "supportsImages": true, "catalogSource": "image_gen_reference" })).collect()
}
fn uncertain(id: Option<&str>) -> Error {
    Error::http(
        502,
        match id {
            Some(id) => format!(
                "科搭生图任务 {id} 的结果未确认，请到官方客户端查看；不会重新提交以免重复扣费"
            ),
            None => "科搭生图提交结果未确认，请到官方客户端核对；不会自动重新生成".into(),
        },
    )
    .with_code("codar_image_uncertain")
}

fn payload(body: &Value) -> Result<Value> {
    let fields = body
        .as_object()
        .ok_or_else(|| Error::http(400, "科搭图片请求必须是 JSON 对象"))?;
    if fields.keys().any(|key| {
        ![
            "model",
            "prompt",
            "size",
            "resolution",
            "n",
            "response_format",
            "output_format",
            "output_compression",
            "quality",
            "user",
            "stream",
            "partial_images",
            "image_urls",
            "mask_url",
        ]
        .contains(&key.as_str())
    }) {
        return Err(Error::http(
            400,
            "科搭此入口只接受官方图片生成参数，上传文件与其他参数尚未接入",
        ));
    }
    // 输出编码与压缩由宿主已有 image_output 处理，不发送给科搭图片工具。
    if body.get("output_format").is_some_and(|value| {
        !value.is_null() && !matches!(value.as_str(), Some("png" | "jpeg" | "webp"))
    }) || body.get("output_compression").is_some_and(|value| {
        !value.is_null()
            && (value.as_u64().is_none_or(|value| value > 100) || body["output_format"] != "jpeg")
    }) || body
        .get("quality")
        .is_some_and(|value| !value.is_null() && value != "auto")
        || body
            .get("user")
            .is_some_and(|value| !value.is_null() && !value.is_string())
        || body
            .get("stream")
            .is_some_and(|value| !value.is_null() && value != false)
        || body
            .get("partial_images")
            .is_some_and(|value| !value.is_null() && value.as_u64() != Some(0))
    {
        return Err(Error::http(
            400,
            "科搭仅支持非流式单图；质量请用 resolution，输出编码由主程序处理",
        ));
    }
    let model = body["model"].as_str().unwrap_or(MODELS[0].0);
    if body.get("response_format").is_some_and(|format| {
        !format.is_null() && !matches!(format.as_str(), Some("url" | "b64_json"))
    }) {
        return Err(Error::http(
            400,
            "科搭 response_format 只接受 url 或 b64_json",
        ));
    }
    if !is_model(model) {
        return Err(Error::http(400, "科搭生图模型不在支持清单内"));
    }
    let prompt = body["prompt"]
        .as_str()
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty() && prompt.chars().count() <= 16000)
        .ok_or_else(|| Error::http(400, "请填写1至16000字的图片描述"))?;
    if body.get("n").is_some_and(|value| value.as_u64() != Some(1)) {
        return Err(Error::http(400, "科搭每次只生成一张图片"));
    }
    let resolution = body["resolution"]
        .as_str()
        .unwrap_or("1K")
        .to_ascii_uppercase();
    if !matches!(resolution.as_str(), "1K" | "2K" | "4K") {
        return Err(Error::http(400, "科搭图片分辨率支持1K、2K、4K"));
    }
    let size = match body["size"].as_str() {
        None | Some("auto") => "16:9",
        Some(size) => size,
    };
    if size.is_empty()
        || size.len() > 32
        || !size
            .bytes()
            .all(|byte| byte.is_ascii_digit() || b":xX".contains(&byte))
    {
        return Err(Error::http(400, "科搭图片尺寸请填写宽高比或宽x高"));
    }
    let mut args =
        json!({ "model": model, "prompt": prompt, "size": size, "resolution": resolution });
    if let Some(images) = body.get("image_urls") {
        let images = images
            .as_array()
            .filter(|images| images.len() <= 16)
            .ok_or_else(|| Error::http(400, "参考图链接最多16个"))?;
        for image in images {
            util::checked_url(
                image
                    .as_str()
                    .ok_or_else(|| Error::http(400, "参考图必须是HTTPS链接"))?,
            )?;
        }
        if !images.is_empty() {
            args["image_urls"] = json!(images);
        }
    }
    if let Some(mask) = body.get("mask_url").filter(|value| !value.is_null()) {
        if !matches!(model, "gpt-image-2" | "gpt-image-2.5")
            || args["image_urls"].as_array().is_none_or(Vec::is_empty)
        {
            return Err(Error::http(400, "遮罩只支持带参考图的GPT Image 2或2.5"));
        }
        util::checked_url(
            mask.as_str()
                .ok_or_else(|| Error::http(400, "遮罩必须是HTTPS链接"))?,
        )?;
        args["mask_url"] = mask.clone();
    }
    Ok(args)
}

fn tool(current: &account::Snapshot, name: &str, arguments: Value, timeout: u64) -> Result<Value> {
    let reply = current.request(
        "POST",
        endpoints::ACCOUNT,
        endpoints::RESEARCH,
        Some(&json!({
            "tool_name": name, "username": current.secret["username"], "arguments": arguments,
        })),
        timeout,
    )?;
    if util::number(&reply["code"]) != Some(200.0) || !reply["data"].is_object() {
        return Err(Error::http(502, "科搭图片工具未返回有效结果"));
    }
    Ok(reply["data"].clone())
}

pub fn generate(params: &Value) -> Result<Value> {
    let body = payload(&params["body"])?;
    // 参考图也须走宿主已有限制的公网图片资源服务，不带本家凭据访问链接。
    let mut references = std::collections::HashSet::new();
    for url in body["image_urls"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(body.get("mask_url").into_iter())
    {
        let raw = url
            .as_str()
            .ok_or_else(|| Error::http(400, "参考图片必须是 HTTPS 链接"))?;
        if references.insert(raw.to_string()) {
            Resource::fetch_public(raw)?.remove()?;
        }
    }
    token::ensure(false)?;
    let current = account::current()?;
    current.verify()?;
    let reply = tool(&current, "image_gen", body, 20_000).map_err(|error| {
        if error
            .status
            .is_some_and(|status| (400..500).contains(&status))
        {
            error
        } else {
            uncertain(None)
        }
    })?;
    let id = reply["task_id"]
        .as_str()
        .or_else(|| reply["result"].as_str())
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        })
        .ok_or_else(|| uncertain(None))?
        .to_string();
    let deadline = util::now()?.saturating_add(300_000);
    while util::now()? < deadline {
        current.unchanged()?;
        let reply = tool(
            &current,
            "get_tool_task_result",
            json!({ "task_id": id }),
            12_000,
        )
        .map_err(|_| uncertain(Some(&id)))?;
        let result = &reply["result"];
        match result["status"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "succeeded" => {
                let url =
                    util::checked_url(result["url"].as_str().ok_or_else(|| uncertain(Some(&id)))?)?;
                let image = Resource::fetch_public(url.as_str())
                    .and_then(Resource::image_result)
                    .map_err(|_| uncertain(Some(&id)))?;
                current.unchanged()?;
                return Ok(
                    json!({ "created": util::now()? / 1000, "data": [image], "taskId": id }),
                );
            }
            "queued" | "pending" | "processing" | "running" | "submitted" | "in_progress" => {
                sdk::sleep(2000)?
            }
            "failed" | "error" | "canceled" | "cancelled" | "expired" | "timeout" | "timed_out" => {
                return Err(Error::http(
                    502,
                    format!(
                        "科搭生图失败：{}",
                        util::safe_text(
                            &util::text(result, &["error", "message"]),
                            &[current.access()?],
                            300
                        )
                    ),
                )
                .with_code("codar_image_failed"));
            }
            _ => return Err(uncertain(Some(&id))),
        }
    }
    Err(uncertain(Some(&id)))
}
