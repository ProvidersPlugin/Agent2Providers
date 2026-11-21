use crate::{json, Value};

pub fn prepare_methods(mut metadata: Value, provider: &str, locale: &str) -> Value {
    let methods = if metadata.is_array() {
        metadata.as_array_mut()
    } else {
        metadata.get_mut("methods").and_then(Value::as_array_mut)
    };
    let Some(methods) = methods else { return metadata; };
    methods.sort_by_key(|method| method["type"] != "oauth" || method["browserMode"] == "none");
    let label = match locale {
        "en" => "Import credential file",
        "ja" => "認証ファイルを取り込む",
        "ko" => "인증 파일 가져오기",
        "pt-BR" => "Importar arquivo de credenciais",
        "zh-Hant" => "匯入憑據檔案",
        _ => "导入凭据文件",
    };
    for method in methods {
        if method["type"] != "api" || method.get("file").is_some() || method["dynamicPrompts"] == true { continue; }
        let field = method["prompts"].as_array().and_then(|prompts| prompts.iter().find(|prompt|
            prompt["type"] == "text" && prompt.get("when").is_none() && matches!(prompt["key"].as_str(),
                Some("credentialJson" | "credentialText" | "credential" | "credentials"))));
        let Some(field) = field else { continue; };
        let key = field["key"].as_str().unwrap_or("").to_string();
        let required = field["optional"] != true;
        let binary = provider == "kuku";
        method["file"] = json!({ "field": key, "accept": if binary { "" } else { ".json,.info,.txt,.bin,.dsh" },
            "maxBytes": if binary { 32 * 1024 * 1024 } else { 262_144 }, "binary": binary });
        if required { method["label"] = json!(label); }
    }
    metadata
}
