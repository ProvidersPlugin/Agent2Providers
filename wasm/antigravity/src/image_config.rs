//! 原生image_generation.rs参数约束：n=1、1K/2K/4K原生比例、14张或旧2.5系列3张参考图。

use super::{catalog_shape, image_input};
use agent2api_wasm_sdk::{json, Error, Result, Value};

fn invalid(message: impl Into<String>) -> Error {
    Error::http(400, message).with_code("local_precondition")
}

pub fn config(body: &Value, model: &str, editing: bool) -> Result<Value> {
    if !catalog_shape::image(model) {
        return Err(invalid("Antigravity 生图请使用 gemini-*-image 模型"));
    }
    if body
        .get("n")
        .is_some_and(|v| !v.is_null() && v.as_u64() != Some(1))
    {
        return Err(invalid("Antigravity 单次生图仅支持 n=1"));
    }
    for (field, allowed) in [
        ("quality", &["auto", "standard"][..]),
        ("background", &["auto", "opaque"][..]),
        ("moderation", &["auto"][..]),
    ] {
        if body
            .get(field)
            .is_some_and(|v| !v.is_null() && v.as_str().is_none_or(|s| !allowed.contains(&s)))
        {
            return Err(invalid(format!(
                "Antigravity 不支持此 {field} 选项，分辨率请通过 size 指定"
            )));
        }
    }
    for field in ["style", "mask", "input_fidelity"] {
        if body.get(field).is_some_and(|v| !v.is_null()) {
            return Err(invalid(format!("Antigravity 不支持 {field} 参数")));
        }
    }
    if body["prompt"].as_str().is_none_or(|s| s.trim().is_empty()) {
        return Err(invalid("Antigravity 图片请求需要提示词"));
    }
    let images = body["images"].as_array();
    if editing && images.is_none_or(|images| images.is_empty()) {
        return Err(invalid("图片编辑需要至少一张参考图"));
    }
    if body
        .get("images")
        .is_some_and(|v| !v.is_null() && !v.is_array())
    {
        return Err(invalid("Antigravity 参考图必须是数组"));
    }
    if let Some(images) = images {
        let max = if model.starts_with("gemini-2.5-") {
            3
        } else {
            14
        };
        if images.len() > max {
            return Err(invalid(format!(
                "该 Antigravity 模型最多支持 {max} 张参考图"
            )));
        }
        for image in images {
            let source = image["image_url"]
                .as_str()
                .filter(|s| s.starts_with("data:image/"))
                .ok_or_else(|| {
                    invalid("Antigravity 图片编辑需要内联参考图，不读取本机路径或隐式下载")
                })?;
            image_input::data_uri(source)?;
        }
    }
    let size = body["size"].as_str().unwrap_or("auto");
    if size == "auto" {
        return Ok(if body.get("images").is_some() {
            json!({ "imageSize": "1K" })
        } else {
            json!({ "aspectRatio": "1:1", "imageSize": "1K" })
        });
    }
    let (width, height) = size
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
        .filter(|(w, h)| *w > 0 && *h > 0)
        .ok_or_else(|| invalid("Antigravity size 必须是 auto 或 WIDTHxHEIGHT"))?;
    let ratio = width as f64 / height as f64;
    let ratios: [(&str, f64); 10] = [
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
    let aspect = ratios
        .iter()
        .min_by(|left, right| (ratio - left.1).abs().total_cmp(&(ratio - right.1).abs()))
        .filter(|(_, candidate)| (ratio - candidate).abs() / candidate <= 0.04)
        .map(|(name, _)| *name)
        .ok_or_else(|| invalid("Antigravity 不支持该宽高比，请选择标准图片比例"))?;
    let resolution = match width.max(height) {
        1..=1792 => "1K",
        1793..=3072 => "2K",
        3073..=4096 => "4K",
        _ => return Err(invalid("Antigravity 图片尺寸最大为4K档")),
    };
    Ok(json!({ "aspectRatio": aspect, "imageSize": resolution }))
}
