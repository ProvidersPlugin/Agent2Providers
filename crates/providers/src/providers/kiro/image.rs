//! Kiro 的图片上限：Base64 超过 5 MiB 的图片在发请求前缩小。
//!
//! ── 依据 ────────────────────────────────────────────────────
//! 上游对**编码后**的 Base64 长度设 5 MiB 上限（报错原文
//! `image exceeds 5 MB maximum: … (IMAGE_SIZE_EXCEEDED)`）。原图不处理的话，
//! 它留在历史里，之后每一轮都会被同一个错误挡住 —— 所以必须就地缩小，
//! 缩不动就换成一句说明，让对话继续。
//!
//! ── 与参考实现的差别 ────────────────────────────────────────
//! 参考实现用 Bun 的图片 API（WebP / JPEG 都能编）。Rust 侧用 `image` crate：
//! 先按长边 2000 重编码原格式（PNG），不行再 JPEG，质量逐步从 80 降到 60。
//! 透明通道在 JPEG 上会被填成黑色背景 —— 截图类图片通常不透明，可接受。

use std::io::Cursor;

/// 上游的 Base64 上限（字符数）。
pub const IMAGE_LIMIT: usize = 5 * 1024 * 1024;

/// 缩小后的长边。
const IMAGE_EDGE: u32 = 2000;

/// 解码前先判尺寸：超过这个像素数直接放弃（防图片炸弹）。
const MAX_PIXELS: u64 = 50_000_000;

/// 过大的输入（Base64 字符数）不再尝试解码。
const MAX_INPUT: usize = 64 * 1024 * 1024;

/// 一次缩小的结果。
pub enum Fitted {
    /// 缩小成功（新的格式与 Base64）。
    Smaller { format: String, bytes: String },
    /// 缩不了（原因，用于替换成文字说明）。
    Note(String),
}

/// 把一张超限的图片缩小到上限以内。
pub fn fit(b64: &str) -> Fitted {
    if b64.len() > MAX_INPUT {
        return Fitted::Note("it is too large to resize".into());
    }
    let raw = match base64_decode(b64) {
        Ok(raw) => raw,
        Err(_) => return Fitted::Note("it couldn't be read as an image".into()),
    };
    let reader = match image::ImageReader::new(Cursor::new(&raw)).with_guessed_format() {
        Ok(reader) => reader,
        Err(_) => return Fitted::Note("it couldn't be read as an image".into()),
    };
    let dimensions = match reader.into_dimensions() {
        Ok(dimensions) => dimensions,
        Err(_) => return Fitted::Note("it couldn't be read as an image".into()),
    };
    if u64::from(dimensions.0) * u64::from(dimensions.1) > MAX_PIXELS {
        return Fitted::Note("it has too many pixels to resize".into());
    }
    let dynamic = match image::load_from_memory(&raw) {
        Ok(dynamic) => dynamic,
        Err(_) => return Fitted::Note("it couldn't be read as an image".into()),
    };
    let long_edge = dimensions.0.max(dimensions.1);
    let target = long_edge.min(IMAGE_EDGE).max(1);
    let resized = if long_edge > target {
        dynamic.resize(target, target, image::imageops::FilterType::Lanczos3)
    } else {
        dynamic
    };
    // 原格式（PNG）优先：保住透明通道与无损画质。
    if let Ok(bytes) = encode_png(&resized) {
        if bytes.len() <= IMAGE_LIMIT {
            return Fitted::Smaller {
                format: "png".into(),
                bytes,
            };
        }
    }
    for quality in [80u8, 70, 60, 50] {
        if let Ok(bytes) = encode_jpeg(&resized, quality) {
            if bytes.len() <= IMAGE_LIMIT {
                return Fitted::Smaller {
                    format: "jpeg".into(),
                    bytes,
                };
            }
        }
    }
    Fitted::Note("it couldn't be made small enough".into())
}

fn encode_png(image: &image::DynamicImage) -> Result<String, ()> {
    let mut buffer = Cursor::new(Vec::new());
    image
        .write_to(&mut buffer, image::ImageFormat::Png)
        .map_err(|_| ())?;
    Ok(base64_encode(buffer.get_ref()))
}

fn encode_jpeg(image: &image::DynamicImage, quality: u8) -> Result<String, ()> {
    let mut buffer = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buffer, quality);
    encoder.encode_image(&image.to_rgb8()).map_err(|_| ())?;
    Ok(base64_encode(&buffer))
}

fn base64_decode(input: &str) -> Result<Vec<u8>, ()> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    let cleaned: String = input.chars().filter(|ch| !ch.is_whitespace()).collect();
    STANDARD.decode(cleaned).map_err(|_| ())
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    STANDARD.encode(bytes)
}

/// 解码一张 data URL 里的 base64（`data:image/png;base64,…`）。
pub fn split_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    if !meta.contains(";base64") {
        return None;
    }
    let media = meta.split(';').next().unwrap_or("image/png");
    let format = media.strip_prefix("image/").unwrap_or("png");
    let format = match format.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "jpeg",
        "webp" => "webp",
        "gif" => "gif",
        "png" => "png",
        _ => "png",
    };
    Some((format.to_string(), data.to_string()))
}
