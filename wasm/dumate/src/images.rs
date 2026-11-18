use agent2api_wasm_sdk::{self as sdk, images::Resource, json, Error, Result, Value};
use url::Url;

use crate::{account, endpoints, network, token, util};

fn uncertain(task_id: Option<&str>) -> Error {
    Error::http(502, match task_id {
        Some(id) => format!("百度搭子生图任务 {id} 的结果未确认，请到官方页面查看；不会自动重新提交以免重复扣费"),
        None => "百度搭子生图提交结果未确认，请到官方页面查看；不会自动重新提交以免重复扣费".into(),
    }).with_code("image_output_uncertain")
}

fn payload(original: &Value) -> Result<Value> {
    if !original.is_object() {
        return Err(Error::http(400, "图片请求必须是 JSON 对象"));
    }
    let model = original
        .get("model")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| Error::http(400, "图片模型必须为文本"))
        })
        .transpose()?
        .unwrap_or("dumate-image2.2");
    if model != "dumate-image2.2" {
        return Err(Error::http(400, "百度搭子文生图目前仅接入 dumate-image2.2"));
    }
    let prompt = original["prompt"]
        .as_str()
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty() && prompt.len() <= 32 * 1024)
        .ok_or_else(|| Error::http(400, "请填写图片描述，长度不能超过 32 KiB"))?;
    let count =
        match original.get("n") {
            Some(value) if !value.is_null() => value
                .as_u64()
                .filter(|count| (1..=4).contains(count))
                .ok_or_else(|| Error::http(400, "百度搭子每次可生成 1 至 4 张图片"))?,
            _ => 1,
        };
    if [
        "images",
        "image",
        "imageUrls",
        "image_urls",
        "mask",
        "mask_url",
        "style",
        "background",
        "input_fidelity",
    ]
    .iter()
    .any(|key| original.get(*key).is_some_and(|value| !value.is_null()))
    {
        return Err(Error::http(
            400,
            "百度搭子此入口只支持文生图，参考图编辑和其他风格参数尚未接入，请写在图片描述中",
        ));
    }
    for (key, choices) in [
        ("output_format", &["png", "jpeg", "webp"][..]),
        ("response_format", &["url", "b64_json"][..]),
    ] {
        if original
            .get(key)
            .filter(|value| !value.is_null())
            .is_some_and(|value| !value.as_str().is_some_and(|value| choices.contains(&value)))
        {
            return Err(Error::http(400, format!("百度搭子不支持该 {key} 参数")));
        }
    }
    if let Some(compression) = original
        .get("output_compression")
        .filter(|value| !value.is_null())
    {
        if !compression.as_u64().is_some_and(|value| value <= 100)
            || original["output_format"] != "jpeg"
        {
            return Err(Error::http(
                400,
                "图片压缩质量须为 0 至 100 的整数且 output_format=jpeg",
            ));
        }
    }
    let quality = original
        .get("quality")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| Error::http(400, "图片质量必须为文本"))
        })
        .transpose()?
        .unwrap_or("low");
    if !matches!(quality, "low" | "high" | "auto") {
        return Err(Error::http(400, "百度搭子图片质量支持 low、high 或 auto"));
    }
    let size = original
        .get("size")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| Error::http(400, "图片尺寸必须为文本"))
        })
        .transpose()?
        .unwrap_or("2K");
    let mut payload = json!({ "query": prompt, "model": model, "quality": if quality == "high" { "high" } else { "low" },
        "maxImagesNum": count });
    if matches!(
        size.to_ascii_uppercase().as_str(),
        "1K" | "2K" | "4K" | "AUTO"
    ) {
        payload["resolution"] = json!(if size.eq_ignore_ascii_case("auto") {
            "2K".into()
        } else {
            size.to_ascii_uppercase()
        });
        payload["aspectRatio"] = json!("1:1");
    } else if let Some((width, height)) = size.split_once('x') {
        if !width
            .parse::<u32>()
            .is_ok_and(|width| (256..=4096).contains(&width))
            || !height
                .parse::<u32>()
                .is_ok_and(|height| (256..=4096).contains(&height))
        {
            return Err(Error::http(
                400,
                "图片尺寸需为 256 至 4096 像素，或选择 1K / 2K / 4K",
            ));
        }
        payload["size"] = json!(size);
    } else {
        return Err(Error::http(400, "图片尺寸请填写宽x高，或选择 1K / 2K / 4K"));
    }
    Ok(payload)
}

fn urls(value: &Value) -> Result<Option<Vec<String>>> {
    let Some(rows) = value.get("imageUrls").filter(|rows| !rows.is_null()) else {
        return Ok(None);
    };
    let rows = rows
        .as_array()
        .filter(|rows| rows.len() <= 4)
        .ok_or_else(|| Error::http(502, "百度搭子生图结果格式异常"))?;
    if rows.is_empty() {
        return Ok(None);
    }
    let mut urls = rows
        .iter()
        .map(|row| {
            let url = row
                .as_str()
                .ok_or_else(|| Error::http(502, "百度搭子返回的图片地址无效"))?;
            let parsed =
                Url::parse(url).map_err(|_| Error::http(502, "百度搭子返回的图片地址无效"))?;
            if url.len() > 8192
                || parsed.scheme() != "https"
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.fragment().is_some()
            {
                return Err(Error::http(502, "百度搭子返回了不安全的图片地址"));
            }
            Ok(url.to_string())
        })
        .collect::<Result<Vec<_>>>()?;
    urls.dedup();
    Ok(Some(urls))
}

pub fn generate(params: &Value) -> Result<Value> {
    let body = payload(&params["body"])?;
    let original = account::current()?;
    token::ensure(false)?;
    let current = account::current()?;
    if current.identity != original.identity
        || current.cookie.header != original.cookie.header
        || current.cookie.origin != original.cookie.origin
    {
        return Err(crate::errors::changed());
    }
    let token = util::text(&current.secret, &["appToken"]);
    current.unchanged()?;
    let protected = current.protected();
    let submitted = network::gateway("POST", endpoints::IMAGE, &token, Some(&body), &protected)
        .map_err(|error| {
            if matches!(
                error.code.as_deref(),
                Some(
                    "dumate_upstream"
                        | "dumate_session_expired"
                        | "dumate_rate_limited"
                        | "dumate_quota_exhausted"
                        | "dumate_content_blocked"
                )
            ) && error
                .status
                .is_some_and(|status| (400..500).contains(&status))
            {
                error
            } else {
                uncertain(None)
            }
        })?;
    current.unchanged()?;
    let id = util::identifier(&submitted["task_id"]).ok_or_else(|| uncertain(None))?;
    let mut url = Url::parse(&format!("{}{}", endpoints::GATEWAY, endpoints::IMAGE))
        .map_err(|_| uncertain(Some(&id)))?;
    url.query_pairs_mut().append_pair("task_id", &id);
    let path = format!("{}?{}", url.path(), url.query().unwrap_or(""));
    let deadline = util::now()? + 120_000;
    while util::now()? < deadline {
        current.unchanged()?;
        let value = network::gateway("GET", &path, &token, None, &protected)
            .map_err(|_| uncertain(Some(&id)))?;
        current.unchanged()?;
        if let Some(urls) = urls(&value)? {
            let mut images = Vec::with_capacity(urls.len());
            for url in urls {
                current.unchanged()?;
                let resource = Resource::fetch_public(&url).map_err(|_| uncertain(Some(&id)))?;
                current.unchanged()?;
                images.push(resource.image_result().map_err(|_| uncertain(Some(&id)))?);
            }
            current.unchanged()?;
            return Ok(json!({ "created": util::now()? / 1000, "data": images, "taskId": id }));
        }
        let status = util::text(&value, &["status"]).to_ascii_lowercase();
        if ["fail", "error", "reject"]
            .iter()
            .any(|marker| status.contains(marker))
        {
            return Err(Error::http(
                400,
                format!(
                    "百度搭子生图失败：{}",
                    util::safe_text(&util::text(&value, &["message", "msg"]), &protected, 300)
                ),
            )
            .with_code("dumate_image_failed"));
        }
        sdk::sleep(1500)?;
    }
    Err(uncertain(Some(&id)))
}
