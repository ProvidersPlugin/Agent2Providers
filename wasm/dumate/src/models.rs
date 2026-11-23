//! 模型来源是 dumate2api 的已记录云端路由，不把客户端别名冒充独立模型。
//! 参考项目记录 /models 404、/model/list 400、/gateway/v2/models 404；
//! 尚未确认新版在线目录，故 model_refresh=false，不声称排除了所有目录接口。

use agent2api_wasm_sdk::{json, Result, Value};

pub fn rows() -> Vec<Value> {
    let mut rows = [
        ("model-text", "DuMate 默认文本档"),
        ("kimi-k31", "Kimi K3.1 Pro · DuMate 路由"),
        ("glm-53", "GLM 5.3 · DuMate 路由"),
        ("glm-53-flash", "GLM 5.3 Flash · DuMate 路由"),
        ("deepseek-v4-pro", "DeepSeek V4 Pro · DuMate 路由"),
        ("deepseek-v41-flash", "DeepSeek V4.1 Flash · DuMate 路由"),
        ("qwen38-flash", "Qwen 3.8 Flash · DuMate 路由"),
    ]
    .iter()
    .map(|(id, name)| {
        json!({ "id": id, "name": name, "supportsToolCall": true,
            "supportsReasoning": true, "supportsImages": true, "supportsJsonMode": false,
            "source": "reference", "verified": false })
    })
    .collect::<Vec<_>>();
    for row in &mut rows {
        if matches!(
            row["id"].as_str(),
            Some("model-text" | "deepseek-v41-flash")
        ) {
            row["contextWindow"] = json!(1_048_576);
            row["maxOutputTokens"] = json!(393_216);
            row["limitsSource"] = json!("dumate2api recorded measurements");
        }
    }
    rows.push(
        json!({ "id": "qianfan/qwen3.5-35b-a3b", "name": "Qwen 3.5 35B A3B · DuMate 千帆模式",
        "supportsToolCall": true, "supportsReasoning": true, "supportsImages": true,
        "supportsJsonMode": true, "contextWindow": 258_048, "maxOutputTokens": 65_536,
        "source": "reference", "verified": false }),
    );
    rows.push(
        json!({ "id": "dumate-image2.2", "name": "DuMate 文生图 2.2",
        "kind": "image", "output_modalities": ["image"],
        "supports_image_generation": true, "supports_image_editing": false,
        "supportsToolCall": false, "supportsReasoning": false, "supportsImages": false,
        "source": "reference", "verified": false }),
    );
    rows
}

pub fn list() -> Result<Value> {
    Ok(json!({ "models": rows(), "meta": { "remote": false, "refreshed_at": 0 } }))
}

pub fn refresh() -> Result<Value> {
    let mut result = list()?;
    result["refreshed"] = json!(false);
    result["count"] = json!(rows().len());
    result["message"] =
        json!("百度搭子目前使用参考项目中的云端路由清单，未确认在线模型目录接口，未执行远程刷新");
    Ok(result)
}
