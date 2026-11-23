//! 图像模型能力取/v3/config标签；国内兜底来自真实派发，不把国际目录混进来。
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
pub fn builtin() -> Vec<Value> {
    [
        (
            "hunyuan-image-alpha",
            "Hunyuan Image Alpha",
            "text-to-image",
        ),
        (
            "hunyuan-image-alpha-edit",
            "Hunyuan Image Alpha Edit",
            "image-to-image",
        ),
    ]
    .into_iter()
    .filter_map(|(id, name, tag)| decorate(&json!({ "id": id, "name": name, "tags": [tag] })))
    .collect()
}
