//! 官方 API-Inference 异步图片协议，沿用原生 images 参数与已确认型号：
//! https://modelscope.ai/docs/model-service/API-Inference/intro
//! POST /v1/images/generations 返回 task_id；随后只 GET /v1/tasks/{id}，绝不重交任务。
//! output_images 为公网对象存储地址，由宿主无凭证安全下载成当前调用的资源，
//! 不把对象存储通配域加入普通 HTTP 权限。大体积 base64 入站仍受宿主消息上限约束。

use agent2api_wasm_sdk::{self as sdk, http, images::Resource, json, Error, Result, Value};

use super::{account, client, errors, Region};

pub fn capabilities(region: Region, id: &str) -> Option<(bool, bool)> {
    // 国际站当前官方 API 模型卡：旧清单中的别名和创改一体型号也走同一异步协议。
    // https://www.modelscope.ai/models/MusePublic/Qwen-image
    // https://www.modelscope.ai/models/Qwen/Qwen-Image-2.1
    // https://www.modelscope.ai/models/Qwen/Qwen-Image-2.1-Turbo
    // 仅补已确认的国际型号，不把模型市场或另一地区的目录当作调用资格。
    if matches!(region, Region::Intl) {
        if ["Qwen/Qwen-Image-2.1", "Qwen/Qwen-Image-2.1-Turbo"]
            .iter()
            .any(|model| model.eq_ignore_ascii_case(id))
        {
            return Some((true, true));
        }
        if id.eq_ignore_ascii_case("MusePublic/Qwen-image") {
            return Some((true, false));
        }
    }
    const GENERATION: &[&str] = &[
        "Qwen/Qwen-Image",
        "Qwen/Qwen-Image-2512",
        "Tongyi-MAI/Z-Image-Turbo",
        "Tongyi-MAI/Z-Image",
    ];
    const EDITING: &[&str] = &[
        "Qwen/Qwen-Image-Edit",
        "Qwen/Qwen-Image-Edit-2509",
        "Qwen/Qwen-Image-Edit-2511",
    ];
    if GENERATION
        .iter()
        .any(|model| model.eq_ignore_ascii_case(id))
    {
        Some((true, false))
    } else if EDITING.iter().any(|model| model.eq_ignore_ascii_case(id)) {
        Some((false, true))
    } else {
        None
    }
}

pub fn decorate(region: Region, id: &str, row: &mut Value) {
    if let Some((generation, editing)) = capabilities(region, id) {
        row["kind"] = json!("image");
        row["output_modalities"] = json!(["image"]);
        row["supports_image_generation"] = json!(generation);
        row["supports_image_editing"] = json!(editing);
        row["supportsToolCall"] = json!(false);
        row["supportsImages"] = json!(editing);
        row["supportsReasoning"] = json!(false);
    }
}

fn request_body(region: Region, params: &Value, editing: bool) -> Result<Value> {
    let input = &params["body"];
    if !input.is_object() {
        return Err(Error::http(400, "图片请求体必须是 JSON 对象"));
    }
    let model = input["model"]
        .as_str()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| Error::http(400, "ModelScope 图片请求缺少模型名称"))?;
    let capabilities = capabilities(region, model)
        .ok_or_else(|| Error::http(400, "该 ModelScope 型号尚未确认支持 Images 接口"))?;
    if (editing && !capabilities.1) || (!editing && !capabilities.0) {
        return Err(Error::http(
            400,
            "该 ModelScope 型号不支持这类图片操作，请选择对应的生图或编辑模型",
        ));
    }
    if input
        .get("n")
        .filter(|value| !value.is_null())
        .is_some_and(|n| n.as_u64() != Some(1))
    {
        return Err(Error::http(400, "ModelScope 异步图片接口单次仅支持 n=1"));
    }
    let prompt = input["prompt"]
        .as_str()
        .filter(|prompt| !prompt.trim().is_empty() && prompt.chars().count() <= 32_000)
        .ok_or_else(|| {
            Error::http(
                400,
                "ModelScope 图片 prompt 必须是非空文本，最多 32000 个字符",
            )
        })?;
    // 图像编码由宿主 image_output 完成；不把它当作上游采样参数，也不能默默忽略非法值。
    if input
        .get("output_format")
        .filter(|value| !value.is_null())
        .is_some_and(|value| {
            !value
                .as_str()
                .is_some_and(|format| matches!(format, "png" | "jpeg" | "webp"))
        })
    {
        return Err(Error::http(
            400,
            "ModelScope 图片输出格式只支持 png、jpeg 或 webp",
        ));
    }
    if input
        .get("output_compression")
        .filter(|value| !value.is_null())
        .is_some_and(|value| {
            value.as_u64().is_none_or(|compression| compression > 100)
                || input["output_format"] != "jpeg"
        })
    {
        return Err(Error::http(
            400,
            "ModelScope output_compression 仅支持 JPEG 的 0 至 100",
        ));
    }
    for (field, allowed) in [
        ("quality", &["auto", "standard"][..]),
        ("background", &["auto", "opaque"][..]),
        ("moderation", &["auto"][..]),
    ] {
        if let Some(value) = input.get(field).filter(|value| !value.is_null()) {
            if !value.as_str().is_some_and(|value| allowed.contains(&value)) {
                return Err(Error::http(
                    400,
                    format!("ModelScope 不支持 {field}={value}"),
                ));
            }
        }
    }
    for field in ["style", "mask", "input_fidelity"] {
        if input.get(field).is_some_and(|value| !value.is_null()) {
            return Err(Error::http(
                400,
                format!("ModelScope 图片接口暂不支持 {field}"),
            ));
        }
    }
    let mut body = json!({ "model": model, "prompt": prompt });
    if input
        .get("size")
        .filter(|value| !value.is_null())
        .is_some_and(|size| !size.is_string())
    {
        return Err(Error::http(400, "ModelScope 图片 size 必须是字符串"));
    }
    if let Some(size) = input["size"].as_str().filter(|value| *value != "auto") {
        let dimensions = size
            .split_once('x')
            .and_then(|(width, height)| {
                Some((width.parse::<u32>().ok()?, height.parse::<u32>().ok()?))
            })
            .filter(|(width, height)| (64..=2048).contains(width) && (64..=2048).contains(height));
        let (width, height) = dimensions.ok_or_else(|| {
            Error::http(400, "ModelScope size 需要 64 至 2048 范围内的 WIDTHxHEIGHT")
        })?;
        body["size"] = json!(format!("{width}x{height}"));
    }
    if editing {
        let maximum = if matches!(region, Region::Intl)
            && ["Qwen/Qwen-Image-2.1", "Qwen/Qwen-Image-2.1-Turbo"]
                .iter()
                .any(|id| id.eq_ignore_ascii_case(model))
        {
            10
        } else {
            3
        };
        let images = input["images"]
            .as_array()
            .filter(|items| !items.is_empty() && items.len() <= maximum)
            .ok_or_else(|| {
                Error::http(
                    400,
                    format!("ModelScope 参考图编辑需要 1 至 {maximum} 张图片"),
                )
            })?;
        let urls = images
            .iter()
            .map(|image| {
                image["image_url"]
                    .as_str()
                    .filter(|url| !url.is_empty())
                    .map(|url| json!(url))
                    .ok_or_else(|| Error::http(400, "ModelScope 参考图缺少 image_url"))
            })
            .collect::<Result<Vec<_>>>()?;
        // 与原生一致，参考图 URL/data URI 原样传入；不下载用户图片、不借用账号外凭据。
        body["image_url"] = json!(urls);
    }
    Ok(body)
}

fn uncertain(id: Option<&str>, message: impl std::fmt::Display) -> Error {
    let task = id.map(|id| format!(" {id}")).unwrap_or_default();
    Error::http(
        502,
        format!("ModelScope 图片任务{task}结果尚未确认：{message}；未重新提交，以免重复消耗额度"),
    )
    .with_code("image_output_uncertain")
}

fn read_json(response: http::Response, id: Option<&str>) -> Result<Value> {
    let bytes = response
        .bytes(1024 * 1024)
        .map_err(|error| uncertain(id, error.message))?;
    serde_json::from_slice(&bytes).map_err(|_| uncertain(id, "上游未返回有效 JSON"))
}

pub fn generate(region: Region, params: &Value, editing: bool) -> Result<Value> {
    let body = request_body(region, params, editing)?;
    let source = account::Snapshot::read(region)?;
    let token = &source.token;
    let response = http::open(
        "POST",
        &format!("{}/images/generations", region.inference_base()),
        json!({ "Content-Type": "application/json", "Authorization": format!("Bearer {token}"),
            "X-ModelScope-Async-Mode": "true" }),
        Some(&body),
    )
    .map_err(|error| uncertain(None, error.message))?;
    source.check()?;
    if !(200..300).contains(&response.status) {
        let status = response.status;
        let bytes = response.bytes(1024 * 1024)?;
        source.check()?;
        let mut error = if status >= 500 {
            uncertain(
                None,
                errors::message(
                    &serde_json::from_slice::<Value>(&bytes)
                        .unwrap_or_else(|_| json!({ "message": String::from_utf8_lossy(&bytes) })),
                ),
            )
        } else {
            errors::from_bytes(region, status, &bytes)
        };
        error.message = error.message.replace(token, "[凭据已隐藏]");
        return Err(error);
    }
    let payload = read_json(response, None)?;
    source.check()?;
    if !client::succeeded(&payload) {
        return Err(uncertain(
            None,
            errors::message(&payload).replace(token, "[凭据已隐藏]"),
        ));
    }
    let id = payload["task_id"]
        .as_str()
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
        .ok_or_else(|| uncertain(None, "创建响应缺少有效 task_id，上游可能已消耗额度"))?;
    let target = format!("{}/tasks/{id}", region.inference_base());
    loop {
        source.check()?;
        let response = http::open(
            "GET",
            &target,
            json!({ "Authorization": format!("Bearer {token}"), "Content-Type": "application/json",
                "X-ModelScope-Task-Type": "image_generation" }),
            None,
        )
        .map_err(|error| uncertain(Some(id), error.message))?;
        source.check()?;
        if !(200..300).contains(&response.status) {
            return Err(uncertain(
                Some(id),
                format!("查询返回 HTTP {}", response.status),
            ));
        }
        let value = read_json(response, Some(id))?;
        source.check()?;
        if !client::succeeded(&value) {
            return Err(uncertain(
                Some(id),
                errors::message(&value).replace(token, "[凭据已隐藏]"),
            ));
        }
        match value["task_status"].as_str() {
            Some("SUCCEED") => {
                let images = value["output_images"]
                    .as_array()
                    .filter(|images| !images.is_empty() && images.len() <= 16)
                    .ok_or_else(|| uncertain(Some(id), "图片任务完成但未返回有效图片列表"))?;
                let mut data = Vec::with_capacity(images.len());
                for image in images {
                    let url = image
                        .as_str()
                        .filter(|url| url.starts_with("https://"))
                        .ok_or_else(|| uncertain(Some(id), "图片任务返回无效图片地址"))?;
                    // 该资源已在宿主侧验证来源、MIME、字节签名与大小，并已封存。
                    let resource = Resource::fetch_public(url)
                        .map_err(|error| uncertain(Some(id), error.message))?;
                    source.check()?;
                    data.push(
                        resource
                            .image_result()
                            .map_err(|error| uncertain(Some(id), error.message))?,
                    );
                }
                let mut result =
                    json!({ "created": sdk::now() / 1000, "data": data, "task_id": id });
                if let Some(usage) = value.get("usage") {
                    result["usage"] = usage.clone();
                }
                source.check()?;
                return Ok(result);
            }
            Some("FAILED" | "CANCELED" | "CANCELLED") => {
                let message = value
                    .get("error")
                    .filter(|value| !value.is_null())
                    .map(errors::message)
                    .or_else(|| {
                        value
                            .get("message")
                            .or_else(|| value.get("error_message"))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| "上游图片任务失败".to_string());
                return Err(uncertain(
                    Some(id),
                    message
                        .replace(token, "[凭据已隐藏]")
                        .chars()
                        .take(3000)
                        .collect::<String>(),
                ));
            }
            Some("PENDING" | "RUNNING" | "WAITING" | "QUEUED" | "PROCESSING") => {
                sdk::sleep(5000).map_err(|error| uncertain(Some(id), error.message))?;
            }
            _ => return Err(uncertain(Some(id), "图片任务返回未知状态")),
        }
    }
}
