use agent2api_wasm_sdk::{self as sdk, http, json, Error, Result, Value};
use sha2::{Digest, Sha256};

use super::{config::Config, errors, fingerprint};

pub const KEY_FIELDS: &[&str] = &["apiKey", "accessToken", "token", "key"];

pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| {
            value[*key]
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or("")
        .to_string()
}

pub fn shape_ok(key: &str) -> bool {
    key.len() <= 8192
        && key.strip_prefix("user_").is_some_and(|rest| {
            !rest.is_empty()
                && rest
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
        })
}

pub fn api_key(secret: &Value) -> Result<String> {
    let key = text(secret, KEY_FIELDS);
    if key.is_empty() {
        return Err(Error::http(
            401,
            "Command Code 账号缺少 API Key，请重新粘贴 user_ 开头的密钥",
        ));
    }
    if !shape_ok(&key) {
        return Err(Error::http(
            401,
            "Command Code API Key 格式无效，请在 commandcode.ai/studio 重新获取",
        ));
    }
    Ok(key)
}

fn tail(key: &str) -> String {
    key.chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

pub fn identity(secret: &Value) -> Result<String> {
    let key = api_key(secret)?;
    // 保留原生账号主键的16位摘要；换备注不应新建另一个账号。
    Ok(format!(
        "commandcode-{}",
        &format!("{:x}", Sha256::digest(key.as_bytes()))[..16]
    ))
}

pub fn view(secret: &Value) -> Value {
    let key = text(secret, KEY_FIELDS);
    let valid = shape_ok(&key);
    let display = text(secret, &["name"]);
    let display = if display.is_empty() || !key.is_empty() && display.contains(&key) {
        format!("Command Code {}", tail(&key))
    } else {
        display
    };
    json!({ "name": display, "tokenTail": tail(&key), "hasApiKey": valid,
        "hasSession": valid, "longLived": true, "canRefresh": false })
}

fn normalize_key(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches(['"', '\'', '`', ',', ';']).trim();
    if let Some(start) = trimmed.find("user_") {
        let rest: String = trimmed[start + 5..]
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '-')
            .collect();
        if !rest.is_empty() {
            return format!("user_{rest}");
        }
    }
    trimmed.chars().filter(|ch| !ch.is_whitespace()).collect()
}

fn credentials(value: &Value, depth: usize) -> Result<(String, String)> {
    if depth > 4 {
        return Err(Error::http(400, "Command Code 凭据嵌套超过限制"));
    }
    if let Some(raw) = value.as_str() {
        return from_text(raw, depth + 1);
    }
    if !value.is_object() {
        return Err(Error::http(
            400,
            "Command Code 凭据必须是 API Key 或 JSON 对象",
        ));
    }
    let name = text(value, &["name"]);
    let key = text(value, KEY_FIELDS);
    if !key.is_empty() {
        return Ok((key, name));
    }
    if let Some(raw) = value["credentialJson"]
        .as_str()
        .filter(|raw| !raw.trim().is_empty())
    {
        let (key, inner_name) = from_text(raw, depth + 1)?;
        return Ok((key, if name.is_empty() { inner_name } else { name }));
    }
    for container in ["inputs", "pluginSecret", "secret", "auth"] {
        if value[container].is_object() {
            let (key, inner_name) = credentials(&value[container], depth + 1)?;
            return Ok((key, if name.is_empty() { inner_name } else { name }));
        }
    }
    // 不扫描JSON元数据中的user_片段，避免将userId/keyName错当成API Key。
    Err(Error::http(
        400,
        "Command Code 凭据 JSON 缺少 apiKey、accessToken、token 或 key 字段",
    ))
}

fn from_text(content: &str, depth: usize) -> Result<(String, String)> {
    let trimmed = content.trim().trim_start_matches('\u{feff}').trim();
    if trimmed.len() > 256 * 1024 {
        return Err(Error::http(400, "Command Code 凭据文本超过大小限制"));
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => credentials(&value, depth),
        Err(_) if trimmed.starts_with(['{', '[']) => {
            Err(Error::http(400, "Command Code 凭据 JSON 格式无效"))
        }
        Err(_) => Ok((trimmed.to_string(), String::new())),
    }
}

pub fn normalize(params: &Value) -> Result<Value> {
    let source = &params["source"];
    let (raw, name) = match source["kind"].as_str() {
        Some("manual") => {
            let outer = &source["payload"];
            if outer
                .get("methodIndex")
                .is_some_and(|index| !matches!(index.as_u64(), Some(0 | 1)))
            {
                return Err(Error::http(400, "Command Code 凭据导入方式无效"));
            }
            credentials(outer, 0)?
        }
        Some("file_text") => from_text(source["content"].as_str().unwrap_or(""), 0)?,
        Some("desktop") => {
            let snapshot =
                sdk::native_call("desktop.credentials.commandcode.v1", "read", json!({}))?;
            from_text(
                snapshot["content"]
                    .as_str()
                    .ok_or_else(|| Error::new("Command Code 本机快照缺少凭据"))?,
                0,
            )?
        }
        Some("file_base64") => {
            return Err(Error::http(
                400,
                "Command Code 不支持二进制文件导入，请粘贴 API Key",
            ))
        }
        _ => {
            return Err(Error::http(
                400,
                "请粘贴 Command Code 的 API Key（user_ 开头）",
            ))
        }
    };
    if raw.len() > 8192 {
        return Err(Error::http(
            400,
            "粘贴的内容过长，请确认复制的是 API Key 本身",
        ));
    }
    let key = normalize_key(&raw);
    if !shape_ok(&key) {
        return Err(Error::http(
            400,
            "Command Code API Key 应以 user_ 开头且只含字母、数字、_ 和 -",
        ));
    }
    let config = Config::load()?;
    // 保留best-effort添加：只有明确鉴权拒绝才拒绝；套餐/额度和暂时网络失败不等于Key无效。
    match http::open_timeout(
        "GET",
        &config.url("/alpha/billing/credits"),
        config.headers(&key),
        None,
        5_000,
    ) {
        Ok(response) if matches!(response.status, 401 | 403) => {
            let status = response.status;
            let body = match response.bytes(256 * 1024) {
                Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({})),
                Err(error)
                    if errors::host_failure(&error) && error.code.as_deref() != Some("timeout") =>
                {
                    return Err(error)
                }
                Err(_) => json!({}),
            };
            if status == 401 || errors::authentication_failure(&body) {
                return Err(Error::http(
                    401,
                    "Command Code API Key 无效或已失效，请在 commandcode.ai/studio 重新获取",
                ));
            }
        }
        Err(error) if errors::host_failure(&error) && error.code.as_deref() != Some("timeout") => {
            return Err(error)
        }
        _ => {}
    }
    let explicit_name = text(params, &["name"]);
    let name = if explicit_name.is_empty() {
        name
    } else {
        explicit_name
    };
    let name = if name.is_empty() {
        format!("Command Code {}", tail(&key))
    } else {
        name
    };
    if name.chars().count() > 256 || name.chars().any(char::is_control) || name.contains(&key) {
        return Err(Error::http(400, "账号备注过长、包含控制字符或完整 API Key"));
    }
    let secret = json!({ "apiKey": key, "name": name });
    // 本机导入为一次快照，后续使用已保存Key，不持续读盘或执行官方客户端。
    Ok(
        json!({ "identity": identity(&secret)?, "public": view(&secret), "secret": secret, "desktop": false }),
    )
}

pub fn unchanged(before: &Value) -> Result<()> {
    let after = sdk::account()?;
    if ["id", "provider", "identity", "secret"]
        .iter()
        .any(|key| before[*key] != after[*key])
    {
        return Err(
            Error::http(409, "Command Code 账号在操作期间已变化，请重新发起")
                .with_code("account_changed"),
        );
    }
    Ok(())
}

pub fn ensure() -> Result<String> {
    let record = sdk::account()?;
    let key = api_key(&record["secret"])?;
    let config = Config::load()?;
    fingerprint::ensure_initialized(&key, &config)?;
    unchanged(&record)?;
    Ok(key)
}

pub fn methods(locale: &str) -> Value {
    let labels = match locale {
        "en" => ["Paste API Key", "Paste a user_ API Key from commandcode.ai/studio.",
            "Import credentials text", "Credentials text", "Select or paste a Command Code credential file or API Key to import the account."],
        "zh-Hant" => ["貼上 API Key", "從 commandcode.ai/studio 複製 user_ 開頭的 API Key 貼上即可。",
            "匯入憑證文字", "憑證文字", "選擇或貼上 Command Code 憑據檔案或 API Key 即可匯入帳號。"],
        "ja" => ["API Key を貼り付け", "commandcode.ai/studio の user_ で始まる API Key を貼り付けてください。",
            "認証情報テキストを取り込む", "認証情報テキスト", "Command Code の認証ファイルまたは API Key を選択・貼り付けてアカウントを取り込めます。"],
        "ko" => ["API Key 붙여넣기", "commandcode.ai/studio의 user_로 시작하는 API Key를 붙여넣으세요.",
            "인증 정보 텍스트 가져오기", "인증 정보 텍스트", "Command Code 인증 파일 또는 API Key를 선택하거나 붙여넣어 계정을 가져올 수 있습니다."],
        "pt-BR" => ["Colar API Key", "Cole a API Key iniciada por user_ de commandcode.ai/studio.",
            "Importar texto de credenciais", "Texto de credenciais", "Selecione ou cole um arquivo de credenciais ou API Key do Command Code para importar a conta."],
        _ => ["粘贴 API Key", "从 commandcode.ai/studio 复制 user_ 开头的 API Key 粘贴即可。",
            "导入凭证文本", "凭证文本", "选择或粘贴 Command Code 凭据文件或 API Key 即可导入账号。"],
    };
    let mut result = json!({ "methods": [
        { "methodIndex": 0, "type": "api", "label": labels[0], "prompts": [
            { "key": "apiKey", "type": "text", "label": "API Key", "secret": true }
        ], "instructions": labels[1] },
        { "methodIndex": 1, "type": "api", "label": labels[2], "prompts": [
            { "key": "credentialJson", "type": "text", "label": labels[3], "secret": true }
        ], "instructions": labels[4] }
    ] });
    if let Some(methods) = result["methods"].as_array_mut() {
        methods.push(sdk::login::desktop_method(2, "Command Code", locale));
    }
    result
}
