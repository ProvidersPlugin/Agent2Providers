//! 官方图片尺寸至少1024²像素、边长16对齐；常用宽高比按原公式保留。

use agent2api_wasm_sdk::{Error, Result, Value};
fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}
pub fn normalize(body: &Value) -> Result<String> {
    let size = body["size"]
        .as_str()
        .filter(|value| *value != "auto")
        .unwrap_or("1024x1024");
    let (width, height) = size
        .split_once('x')
        .and_then(|(width, height)| Some((width.parse::<u32>().ok()?, height.parse::<u32>().ok()?)))
        .filter(|(width, height)| (1..=16_384).contains(width) && (1..=16_384).contains(height))
        .ok_or_else(|| Error::http(400, "小浣熊size需要正整数WIDTHxHEIGHT"))?;
    for (rw, rh) in [
        (1, 1),
        (3, 4),
        (4, 3),
        (16, 9),
        (9, 16),
        (3, 2),
        (2, 3),
        (21, 9),
    ] {
        if width * rh != height * rw {
            continue;
        }
        let wa = 16 / gcd(rw, 16);
        let ha = 16 / gcd(rh, 16);
        let factor = wa / gcd(wa, ha) * ha;
        let uw = rw * factor;
        let uh = rh * factor;
        let pixels = (1_048_576.0 / f64::from(uw * uh)).sqrt().ceil() as u32;
        let units = width
            .div_ceil(uw)
            .max(height.div_ceil(uh))
            .max(pixels)
            .max(1);
        return Ok(format!("{}x{}", uw * units, uh * units));
    }
    let scale = (1_048_576.0 / (f64::from(width) * f64::from(height)))
        .sqrt()
        .max(1.0);
    let aligned = |value| (f64::from(value) * scale / 16.0).ceil() as u32 * 16;
    Ok(format!("{}x{}", aligned(width), aligned(height)))
}
