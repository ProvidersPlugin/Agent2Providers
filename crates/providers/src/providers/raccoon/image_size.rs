//! 官方图片工具要求至少 1024² 像素、边长按 16 对齐，并保留常见宽高比。

use agent2api_provider_sdk::control_error;
use serde_json::Value;

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

pub(super) fn normalize(body: &Value) -> Result<String, String> {
    let size = body
        .get("size")
        .and_then(Value::as_str)
        .filter(|value| *value != "auto")
        .unwrap_or("1024x1024");
    let (width, height) = size
        .split_once('x')
        .and_then(|(width, height)| Some((width.parse::<u32>().ok()?, height.parse::<u32>().ok()?)))
        .filter(|(width, height)| (1..=16_384).contains(width) && (1..=16_384).contains(height))
        .ok_or_else(|| control_error(400, "小浣熊 size 需要正整数 WIDTHxHEIGHT", None, None))?;
    let ratios = [
        (1, 1),
        (3, 4),
        (4, 3),
        (16, 9),
        (9, 16),
        (3, 2),
        (2, 3),
        (21, 9),
    ];
    for (rw, rh) in ratios {
        if width * rh != height * rw {
            continue;
        }
        let wa = 16 / gcd(rw, 16);
        let ha = 16 / gcd(rh, 16);
        let factor = wa / gcd(wa, ha) * ha;
        let uw = rw * factor;
        let uh = rh * factor;
        let pixels = (1_048_576.0 / (uw * uh) as f64).sqrt().ceil() as u32;
        let units = width
            .div_ceil(uw)
            .max(height.div_ceil(uh))
            .max(pixels)
            .max(1);
        return Ok(format!("{}x{}", uw * units, uh * units));
    }
    let scale = (1_048_576.0 / (width as f64 * height as f64))
        .sqrt()
        .max(1.0);
    let aligned = |value: u32| (value as f64 * scale / 16.0).ceil() as u32 * 16;
    Ok(format!("{}x{}", aligned(width), aligned(height)))
}
