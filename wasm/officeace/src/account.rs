//! 原 identity：officeace- + FNV-1a64(旧规范基址|appKey) 前12个hex，存量身份不变。
//! 当前宿主尚未接入 OfficeAce 固定文件读取；提供云端授权与用户明确手选的JSON导入。

use super::{
    credentials::{self, Credentials},
    dpop::DpopKey,
    endpoints,
    util::text,
};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub fn identity(secret: &Value) -> String {
    let key = text(secret, &["modelAppKey", "appKey", "model_app_key"]);
    if key.is_empty() {
        return String::new();
    }
    let base = endpoints::normalize_base(&text(
        secret,
        &["baseUrl", "base_url", "api_base", "apiBase"],
    ));
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in format!("{base}|{key}").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("officeace-{}", &format!("{hash:016x}")[..12])
}

pub fn view(secret: &Value) -> Value {
    let credential = Credentials::from_secret(secret);
    let id = identity(secret);
    let explicit = text(
        secret,
        &["name", "email", "accountName", "principalId", "accountId"],
    );
    let name = if !explicit.is_empty() {
        explicit
    } else if !id.is_empty() {
        format!(
            "OfficeAce {}",
            id.chars()
                .rev()
                .take(8)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<String>()
        )
    } else {
        "OfficeAce 账号".into()
    };
    let mut public = json!({ "name": name, "baseUrl": credential.base_url,
        "hasControlPlane": credential.has_control(), "hasSession": !credential.model_app_key.is_empty() && !credential.model_app_secret.is_empty(),
        "canRefresh": credentials::can_refresh(secret), "hasRefreshToken": !text(secret, &["refreshToken", "refresh_token"]).is_empty() });
    if credential.expires_at > 0 {
        public["expiresAt"] = json!(credential.expires_at);
    }
    // account.view 也可能面对历史手填记录；不能依赖宿主识别 dpopKeyPair 内的 d 字段。
    if contains_private(&public, secret) {
        public["name"] = json!("OfficeAce 账号");
        public["baseUrl"] = Value::Null;
    }
    // 不展示任何 appKey 前缀/完整值，也不把 DPoP 私钥或一次性票据并入 public。
    public
}

pub fn normalized(mut secret: Value, name: Option<&str>) -> Result<Value> {
    let credential = Credentials::from_secret(&secret);
    credential.forwarding()?;
    secret["baseUrl"] = json!(endpoints::official_base(&credential.base_url)?);
    if !secret.is_object() || secret.to_string().len() > 128 * 1024 {
        return Err(Error::http(400, "OfficeAce 凭据内容过大或格式无效"));
    }
    if let Some(pair) = secret.get("dpopKeyPair") {
        DpopKey::from_json(pair)?;
    }
    if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
        if name.contains('\0') || name.chars().count() > 100 {
            return Err(Error::http(400, "账号名称过长或包含无效字符"));
        }
        secret["name"] = json!(name);
    }
    let public = view(&secret);
    if contains_private(&public, &secret) {
        return Err(Error::http(
            400,
            "账号公开名称或地址包含凭据内容，请移除敏感信息",
        ));
    }
    Ok(
        json!({ "identity": identity(&secret), "public": public, "secret": secret, "desktop": false }),
    )
}

fn contains_private(public: &Value, secret: &Value) -> bool {
    let public = public.to_string();
    let credential = Credentials::from_secret(secret);
    let mut values = credential
        .secrets()
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    values.push(text(secret, &["refreshToken", "refresh_token"]));
    for key in ["private_key_jwk", "privateKeyJwk"] {
        values.push(text(&secret["dpopKeyPair"][key], &["d"]));
    }
    values
        .iter()
        .filter(|value| !value.is_empty())
        .any(|value| public.contains(value))
}

pub fn normalize(params: &Value) -> Result<Value> {
    let source = &params["source"];
    let payload = match source["kind"].as_str() {
        Some("manual") => {
            let payload = &source["payload"];
            match payload["methodIndex"].as_u64() {
                None => payload.clone(),
                Some(index @ (1 | 2)) => {
                    let inputs = payload["inputs"]
                        .as_object()
                        .filter(|inputs| {
                            inputs.iter().all(|(key, value)| {
                                value.is_string()
                                    && if index == 1 {
                                        matches!(
                                            key.as_str(),
                                            "baseUrl" | "modelAppKey" | "modelAppSecret"
                                        )
                                    } else {
                                        key == "credentialJson"
                                    }
                            })
                        })
                        .ok_or_else(|| Error::http(400, "OfficeAce 手动凭据字段无效"))?;
                    if index == 1 {
                        Value::Object(inputs.clone())
                    } else {
                        serde_json::from_str(
                            inputs
                                .get("credentialJson")
                                .and_then(Value::as_str)
                                .unwrap_or(""),
                        )
                        .map_err(|_| Error::http(400, "OfficeAce 凭据不是合法 JSON"))?
                    }
                }
                _ => return Err(Error::http(400, "网页登录不能用于手动凭据导入")),
            }
        }
        Some("file_text") => serde_json::from_str(source["content"].as_str().unwrap_or(""))
            .map_err(|_| {
                Error::http(
                    400,
                    "请选择含 baseUrl、modelAppKey、modelAppSecret 的 JSON 文件",
                )
            })?,
        Some("desktop") => {
            return Err(Error::http(
                400,
                "OfficeAce 本机自动读取尚未接入，请网页登录或选择官方导出的模型凭据 JSON",
            ))
        }
        Some("file_base64") => return Err(Error::http(400, "OfficeAce 不支持二进制凭据文件")),
        _ => return Err(Error::http(400, "OfficeAce 不支持该凭据来源")),
    };
    let payload = imported_payload(payload)?;
    let credential = Credentials::from_secret(&payload);
    credential.forwarding()?;
    let mut secret = json!({ "baseUrl": credential.base_url, "modelAppKey": credential.model_app_key,
        "modelAppSecret": credential.model_app_secret });
    for (key, value) in [
        ("accessKeyId", credential.signing.access_key_id),
        ("secretAccessKey", credential.signing.secret_access_key),
        ("securityToken", credential.signing.security_token),
        ("projectId", credential.signing.project_id),
    ] {
        if !value.is_empty() {
            secret[key] = json!(value);
        }
    }
    if credential.expires_at > 0 {
        secret["expiresAt"] = json!(credential.expires_at);
    }
    for (key, aliases) in [
        ("refreshToken", &["refreshToken", "refresh_token"][..]),
        ("accountId", &["accountId"][..]),
        ("principalId", &["principalId"][..]),
        ("accountName", &["accountName"][..]),
    ] {
        let value = text(&payload, aliases);
        if !value.is_empty() {
            secret[key] = json!(value);
        }
    }
    if let Some(pair) = payload.get("dpopKeyPair").filter(|pair| pair.is_object()) {
        secret["dpopKeyPair"] = pair.clone();
    }
    normalized(
        secret,
        params["name"].as_str().or_else(|| payload["name"].as_str()),
    )
}

fn imported_payload(payload: Value) -> Result<Value> {
    use base64::Engine;
    if !payload.is_object() || payload.to_string().len() > 128 * 1024 {
        return Err(Error::http(400, "OfficeAce 凭据 JSON 格式无效或内容过大"));
    }
    let mut candidates = Vec::new();
    if !text(&payload, &["baseUrl", "base_url", "api_base", "apiBase"]).is_empty() {
        candidates.push(payload.clone());
    } else {
        // 官方 models.json 的 defaults / 分组 entries 是用户本次手选文件，不扫描本机其它账号。
        for (key, value) in payload.as_object().into_iter().flatten() {
            if key == "_meta" {
                continue;
            }
            let rows = value.as_array().or_else(|| value["entries"].as_array());
            for row in rows.into_iter().flatten() {
                candidates.push(
                    row.get("model_client_config")
                        .or_else(|| row.get("modelClientConfig"))
                        .unwrap_or(row)
                        .clone(),
                );
            }
        }
    }
    let mut selected: Option<Value> = None;
    for mut candidate in candidates {
        let base = text(&candidate, &["baseUrl", "base_url", "api_base", "apiBase"]);
        if base.is_empty() {
            continue;
        }
        let base = endpoints::official_base(&base)?;
        if text(&candidate, &["modelAppKey", "appKey", "model_app_key"]).is_empty()
            || text(
                &candidate,
                &["modelAppSecret", "appSecret", "model_app_secret"],
            )
            .is_empty()
        {
            let headers = candidate
                .get("custom_headers")
                .or_else(|| candidate.get("customHeaders"))
                .or_else(|| candidate.get("headers"));
            let mut authorizations: Vec<&str> = headers
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .filter_map(|(_, value)| value.as_str())
                .collect();
            if let Some(value) = candidate.get("authorization").and_then(Value::as_str) {
                authorizations.push(value);
            }
            let authorization = authorizations.first().copied().unwrap_or("").trim();
            if authorizations
                .iter()
                .any(|value| value.trim() != authorization)
            {
                return Err(Error::http(400, "OfficeAce JSON 包含相互冲突的模型鉴权头"));
            }
            let Some((scheme, encoded)) = authorization.split_once(' ') else {
                continue;
            };
            if !scheme.eq_ignore_ascii_case("basic") {
                continue;
            }
            let raw = base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .map_err(|_| Error::http(400, "OfficeAce 模型 Basic 凭据编码无效"))?;
            let raw = std::str::from_utf8(&raw)
                .map_err(|_| Error::http(400, "OfficeAce 模型 Basic 凭据格式无效"))?;
            let (key, secret) = raw
                .split_once(':')
                .ok_or_else(|| Error::http(400, "OfficeAce 模型 Basic 凭据不完整"))?;
            candidate["modelAppKey"] = json!(key);
            candidate["modelAppSecret"] = json!(secret);
        }
        candidate["baseUrl"] = json!(base);
        let credential = Credentials::from_secret(&candidate);
        credential.forwarding()?;
        if selected
            .as_ref()
            .is_some_and(|old| !credential.same_model(&Credentials::from_secret(old)))
        {
            return Err(Error::http(
                400,
                "此 JSON 包含多组不同账号的模型凭据，请单独选择要导入的账号",
            ));
        }
        if selected.is_none() {
            selected = Some(candidate);
        }
    }
    selected.ok_or_else(|| Error::http(400, "OfficeAce JSON 未包含完整官方模型网关与 Basic 凭据"))
}

pub fn snapshot() -> Result<(Value, Credentials)> {
    let record = sdk::account()?;
    let credential = Credentials::from_secret(&record["secret"]);
    credential.forwarding()?;
    Ok((record, credential))
}

pub fn same_model(record: &Value) -> Result<Value> {
    let (latest, actual) = snapshot()?;
    let expected = Credentials::from_secret(&record["secret"]);
    if latest["id"] != record["id"]
        || latest["provider"] != record["provider"]
        || latest["identity"] != record["identity"]
        || latest["accountMaintenanceCall"] != true
            && latest["public"]["enabled"] != record["public"]["enabled"]
        || !actual.same_model(&expected)
    {
        return Err(
            Error::http(409, "OfficeAce 账号或模型凭据已变化，请重新发起操作")
                .with_code("officeace_account_changed"),
        );
    }
    Ok(latest)
}

pub fn unchanged(record: &Value) -> Result<Value> {
    let latest = same_model(record)?;
    if latest["secret"] != record["secret"] {
        return Err(
            Error::http(409, "OfficeAce 凭据已更新，未继续使用旧授权材料")
                .with_code("officeace_account_changed"),
        );
    }
    Ok(latest)
}

pub fn update(record: &Value, next: &Value) -> Result<bool> {
    let mut public = view(next);
    if let Some(name) = record["public"]["name"]
        .as_str()
        .filter(|name| !name.is_empty())
    {
        public["name"] = json!(name);
    }
    if contains_private(&public, next) {
        return Err(Error::http(
            400,
            "账号公开名称包含受保护凭据，未写入公开信息",
        ));
    }
    let result = sdk::host(
        "account.update",
        json!({ "expect": record["secret"], "secret": next, "public": public }),
    )?;
    Ok(result["updated"] == true)
}
