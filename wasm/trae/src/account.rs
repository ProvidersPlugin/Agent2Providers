//! 原生 uid 身份 trae-<variant>-<uid> 不变。原生无uid分支直接拼整枚token，
//! 新增无uid导入改为摘要，旧危险identity需宿主迁移别名，不在公开视图继续传播。
//! 原生Desktop明确不支持、没有已确认的受限本机读取来源；不虚构helper或读盘。

use super::{credentials, util};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub fn identity(secret: &Value) -> String {
    let variant = credentials::variant(secret);
    let uid = util::scalar(&secret["uid"]);
    if !uid.is_empty() {
        return format!("trae-{variant}-{uid}");
    }
    let token = util::text(secret, &["accessToken", "refreshToken"]);
    format!("trae-{variant}-key-{}", util::digest(token.as_bytes()))
}

pub fn view(secret: &Value) -> Value {
    let token = util::text(secret, &["accessToken"]);
    let tail: String = token
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let variant = credentials::variant(secret);
    let name = util::text(secret, &["name", "nickname"]);
    let mut value = json!({ "name": if name.is_empty() { format!("Trae {variant}") } else { util::safe_text(&name, 100) },
        "uid": util::safe_text(&util::scalar(&secret["uid"]), 480),
        "nickname": util::safe_text(&util::text(secret, &["nickname"]), 100),
        "enterpriseId": util::safe_text(&util::scalar(&secret["enterpriseId"]), 480),
        "variant": variant, "tokenTail": tail, "hasRefreshToken": !util::text(secret, &["refreshToken"]).is_empty(),
        "canRefresh": !util::text(secret, &["refreshToken"]).is_empty(),
        "hasAccessToken": !token.is_empty(), "available": !token.is_empty(), "desktop": false });
    if let Some(at) = credentials::expiry(secret) {
        value["expiresAt"] = json!(at);
        value["expiresAtMs"] = json!(at);
    }
    value
}

pub fn normalized(mut secret: Value, source: &str, name: Option<&str>) -> Result<Value> {
    secret["source"] = json!(source);
    if let Some(name) = name.map(str::trim).filter(|v| !v.is_empty()) {
        secret["name"] = json!(util::safe_text(name, 100));
    }
    if let Some(at) = credentials::expiry(&secret) {
        secret["expiresAt"] = json!(at);
    }
    let id = identity(&secret);
    if id.len() > 512 || id.chars().any(char::is_control) {
        return Err(Error::http(400, "Trae 账号身份格式无效"));
    }
    Ok(json!({ "identity": id, "public": view(&secret), "secret": secret, "desktop": false }))
}

fn parse_text(text: &str) -> Result<Value> {
    let text = text.trim();
    if text.is_empty() || text.len() > 256 * 1024 {
        return Err(Error::http(400, "Trae 凭据文本为空或超过限制"));
    }
    if text.starts_with('{') {
        serde_json::from_str(text).map_err(|_| Error::http(400, "Trae 凭据 JSON 格式无效"))
    } else {
        Ok(json!({ "accessToken": text }))
    }
}

pub fn normalize(params: &Value) -> Result<Value> {
    let source = &params["source"];
    let mut payload = match source["kind"].as_str() {
        Some("manual") => {
            let raw = &source["payload"];
            if let Some(index) = raw.get("methodIndex") {
                if !matches!(index.as_u64(), Some(1 | 2)) {
                    return Err(Error::http(400, "Trae 此登录方式必须从原授权入口发起"));
                }
                if !raw["inputs"].is_object() {
                    return Err(Error::http(400, "Trae 凭据表单输入必须是字段对象"));
                }
            }
            raw.get("inputs")
                .filter(|v| v.is_object())
                .unwrap_or(raw)
                .clone()
        }
        Some("file_text") => parse_text(source["content"].as_str().unwrap_or(""))?,
        Some("desktop") => {
            return Err(Error::http(
                503,
                "Trae 本机加密登录态尚无已授权导入能力，请粘贴凭据 JSON",
            ))
        }
        _ => {
            return Err(Error::http(
                400,
                "Trae 仅接受手工凭据或 JSON 内容，不接受本机路径",
            ))
        }
    };
    if let Some(text) = payload.as_str() {
        payload = parse_text(text)?;
    }
    let embedded = util::text(&payload, &["credentialJson"]);
    if !embedded.is_empty() {
        payload = parse_text(&embedded)?;
    }
    let token = util::text(&payload, &["accessToken"]);
    if token.starts_with('{') {
        payload = parse_text(&token)?;
    }
    let mut secret = credentials::parse(&payload)?;
    // GetUserInfo仅补显示/去重；鉴权导入动作已由主程序执行，不从公开回调接refreshToken。
    if util::text(&secret, &["uid"]).is_empty() && !util::text(&secret, &["accessToken"]).is_empty()
    {
        if let Ok(profile) = super::profile::fetch(&secret) {
            for key in ["uid", "nickname", "enterpriseId"] {
                if !util::text(&profile, &[key]).is_empty() {
                    secret[key] = profile[key].clone();
                }
            }
        }
    }
    normalized(
        credentials::parse(&secret)?,
        "manual",
        params["name"].as_str(),
    )
}

pub fn current() -> Result<Value> {
    let record = sdk::account()?;
    credentials::parse(&record["secret"])?;
    Ok(record)
}

pub fn unchanged(secret: &Value) -> Result<()> {
    if sdk::account()?["secret"] != *secret {
        return Err(
            Error::http(409, "Trae 账号凭据已被替换，未覆盖新的登录状态")
                .with_code("local_precondition"),
        );
    }
    Ok(())
}
