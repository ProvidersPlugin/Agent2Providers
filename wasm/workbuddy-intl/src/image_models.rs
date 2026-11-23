//! 原生国际图片通道存在，但没有经确认的国际静态图片清单；只取国际目录实际下发的标签。
use agent2api_wasm_sdk::{json, Value};
pub fn capabilities(row: &Value) -> Option<(bool, bool)> {
    let tags = row["tags"].as_array()?;
    let generation = tags.iter().any(|tag| tag == "text-to-image");
    let editing = tags.iter().any(|tag| tag == "image-to-image");
    (generation || editing).then_some((generation, editing))
}
pub fn decorate(row: &Value) -> Option<Value> {
    let (generation, editing) = capabilities(row)?;
    let mut row = row.clone();
    row["kind"] = json!("image");
    row["output_modalities"] = json!(["image"]);
    row["supports_image_generation"] = json!(generation);
    row["supports_image_editing"] = json!(editing);
    row["supportsImages"] = json!(editing);
    row["supportsToolCall"] = json!(false);
    row["supportsReasoning"] = json!(false);
    Some(row)
}
