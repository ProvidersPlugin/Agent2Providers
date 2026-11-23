//! 纯Rust、纯内存图片压缩，沿用原生5MiB base64上限与2000长边/PNG→JPEG策略。
//! 不读本机文件；公共URL仅交宿主无凭据资源下载。16MiB入站ABI仍未被资源化解决。
//! 原生50MP可超128MiB沙箱：超限压缩仅允许6MP且解码预算32MiB，明确报错不静默丢图。

use ::image::ImageDecoder;
use agent2api_wasm_sdk::{images::Resource, Error, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::io::Cursor;

pub const IMAGE_LIMIT: usize = 5 * 1024 * 1024;
const MAX_RAW: usize = 8 * 1024 * 1024;
const MAX_RESIZE_PIXELS: u64 = 6_000_000;
fn bad(message: &str) -> Error {
    Error::http(400, message).with_code("kiro_image_invalid")
}
fn format(raw: &[u8]) -> Result<&'static str> {
    match ::image::guess_format(raw).map_err(|_| bad("Kiro 图片格式无法识别"))? {
        ::image::ImageFormat::Png => Ok("png"),
        ::image::ImageFormat::Jpeg => Ok("jpeg"),
        ::image::ImageFormat::WebP => Ok("webp"),
        ::image::ImageFormat::Gif => Ok("gif"),
        _ => Err(bad("Kiro 只接收 PNG、JPEG、WebP 或 GIF 图片")),
    }
}
fn fitted(raw: Vec<u8>) -> Result<(String, String)> {
    if raw.is_empty() || raw.len() > MAX_RAW {
        return Err(bad("Kiro 图片超过受控读取上限"));
    }
    let source_format = format(&raw)?;
    let reader = ::image::ImageReader::new(Cursor::new(&raw))
        .with_guessed_format()
        .map_err(|_| bad("Kiro 图片无法解析"))?;
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| bad("Kiro 图片尺寸无效"))?;
    if width == 0 || height == 0 {
        return Err(bad("Kiro 图片尺寸为空"));
    }
    let encoded = STANDARD.encode(&raw);
    if encoded.len() <= IMAGE_LIMIT {
        return Ok((source_format.into(), encoded));
    }
    drop(encoded);
    if u64::from(width) * u64::from(height) > MAX_RESIZE_PIXELS {
        return Err(Error::http(
            413,
            "Kiro 超大图片需要先缩小；当前沙箱无法安全压缩此像素数，未丢弃图片",
        )
        .with_code("kiro_image_resource_limit"));
    }
    let mut reader = ::image::ImageReader::new(Cursor::new(&raw))
        .with_guessed_format()
        .map_err(|_| bad("Kiro 图片无法解析"))?;
    let mut limits = ::image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    let decoder = reader
        .into_decoder()
        .map_err(|_| bad("Kiro图片无法在内存上限内解码"))?;
    if decoder.total_bytes() > 24 * 1024 * 1024 {
        return Err(Error::http(
            413,
            "Kiro图片解码后超过安全内存上限，请先缩小图片",
        ));
    }
    let dynamic = ::image::DynamicImage::from_decoder(decoder)
        .map_err(|_| bad("Kiro 图片无法在内存上限内解码"))?;
    drop(raw);
    let resized = if width.max(height) > 2000 {
        let target_height =
            ((u64::from(height) * 2000).div_ceil(u64::from(width.max(height))) + 1).max(1);
        // ::image::resize会申请width*targetHeight*16的浮点中间图；不能只限制解码尺寸。
        let smaller = if u64::from(width) * target_height * 16 <= 24 * 1024 * 1024 {
            dynamic.resize(2000, 2000, ::image::imageops::FilterType::Lanczos3)
        } else {
            dynamic.thumbnail(2000, 2000)
        };
        drop(dynamic);
        smaller
    } else {
        dynamic
    };
    let mut png = Cursor::new(Vec::new());
    if resized
        .write_to(&mut png, ::image::ImageFormat::Png)
        .is_ok()
    {
        let encoded = STANDARD.encode(png.get_ref());
        if encoded.len() <= IMAGE_LIMIT {
            return Ok(("png".into(), encoded));
        }
    }
    drop(png);
    let rgb = resized.to_rgb8();
    drop(resized);
    for quality in [80u8, 70, 60, 50] {
        let mut buffer = Vec::new();
        let mut encoder =
            ::image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buffer, quality);
        if encoder.encode_image(&rgb).is_err() {
            continue;
        }
        let encoded = STANDARD.encode(buffer);
        if encoded.len() <= IMAGE_LIMIT {
            return Ok(("jpeg".into(), encoded));
        }
    }
    Err(Error::http(
        413,
        "Kiro 图片压缩后仍超过单张上限，请先缩小图片",
    ))
}
pub fn prepare(url: &str) -> Result<String> {
    let raw = if let Some(rest) = url.strip_prefix("data:") {
        let (meta, encoded) = rest
            .split_once(',')
            .ok_or_else(|| bad("Kiro 图片data地址格式无效"))?;
        if !meta.starts_with("image/")
            || !meta.ends_with(";base64")
            || encoded.len() > (MAX_RAW * 4 / 3 + 4096)
        {
            return Err(bad("Kiro 图片须为有界base64图片数据"));
        }
        let cleaned: String = encoded.chars().filter(|ch| !ch.is_whitespace()).collect();
        STANDARD
            .decode(cleaned)
            .map_err(|_| bad("Kiro 图片base64无效"))?
    } else {
        let resource = Resource::fetch_public(url)?;
        let result = (|| -> Result<Vec<u8>> {
            let mut raw = Vec::new();
            loop {
                let bytes = resource.read(raw.len())?;
                if bytes.is_empty() {
                    break;
                }
                if raw.len().saturating_add(bytes.len()) > MAX_RAW {
                    return Err(bad("Kiro 公共图片超过读取上限"));
                }
                raw.extend(bytes);
            }
            Ok(raw)
        })();
        let removed = resource.remove();
        let raw = result?;
        removed?;
        raw
    };
    let (format, bytes) = fitted(raw)?;
    Ok(format!("data:image/{format};base64,{bytes}"))
}
