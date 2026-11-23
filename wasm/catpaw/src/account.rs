//! 手工/文本导入只做静态校验，不验证登录有效性、不触发目录或积分请求。
//! 业务身份不加 provider 前缀；宿主负责保留旧主键与生成新主键。

use agent2api_wasm_sdk::{json, Result, Value};

use crate::{credentials::{self, Credentials}, endpoints, errors, gates, util};

pub fn identity(secret: &Value) -> Result<String> {
    if desktop_reference(secret) { return Ok(endpoints::DESKTOP_ID.into()); }
    Credentials::parse(secret)?.identity()
}

pub fn view(secret: &Value) -> Result<Value> {
    if desktop_reference(secret) {
        return crate::desktop::public(secret);
    }
    let payload = credentials::payload(secret)?;
    let c = Credentials::parse(&payload)?;
    c.identity()?;
    let explicit_name = util::text(&payload, &["name"]);
    let name = if explicit_name.is_empty() {
        if c.login_name.is_empty() { c.user() } else { &c.login_name }
    } else { &explicit_name };
    let secrets = c.secrets();
    let has_token = !c.token.is_empty();
    let tail = token_tail(&c.token);
    let tail = if errors::contains_secret(&tail, &secrets) { String::new() } else { tail };
    let mut public = json!({ "name": errors::public_text(name, &secrets, 100),
        "uid": errors::public_text(&c.uid, &secrets, endpoints::MAX_USER_CHARS),
        "loginName": errors::public_text(&c.login_name, &secrets, endpoints::MAX_USER_CHARS),
        "tokenTail": tail, "hasToken": has_token,
        "hasBalanceToken": c.balance_token.is_some(), "desktop": false,
        "source": match payload["source"].as_str() { Some("file") => "file", Some("browser") => "browser", _ => "manual" },
        "available": has_token, "canRefresh": false, "validation": "format_only",
        "migrationStage": "complete", "availabilityMessage": gates::SUMMARY });
    if !has_token {
        public["reason"] = json!("CatPaw 主登录凭据缺失；已有旧余额凭据仅可作为本账号积分查询的回退来源");
    }
    if c.ignored_balance || payload["balanceCredentialIgnored"] == true {
        public["importWarning"] = json!("无效的可选旧余额凭据已忽略；主登录凭据仍按快照保留");
    }
    Ok(public)
}

fn token_tail(token: &str) -> String {
    // 太短时不展示整个凭据。正常尾号保持原生的 4 字符，而不是宿主允许的上限 6。
    if token.chars().count() <= 4 { return String::new(); }
    token.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect()
}

fn desktop_reference(value: &Value) -> bool {
    credentials::is_desktop(value) || credentials::is_desktop(&value["secret"])
        || credentials::is_desktop(&value["pluginSecret"])
}

pub fn normalize(params: &Value) -> Result<Value> {
    credentials::own_provider(params)?;
    if params["source"]["kind"] == "desktop" { return crate::desktop::normalize(params); }
    let (value, source, form_name) = input(params)?;
    let value = credentials::payload(&value)?;
    let c = Credentials::parse(&value)?;
    c.require_main()?;
    let identity = c.identity()?;
    let explicit = params["name"].as_str().map(str::trim).filter(|name| !name.is_empty())
        .map(str::to_string).or(form_name);
    let default = util::text(&value, &["name"]);
    let default = if default.is_empty() { util::text(&value["account"], &["name"]) } else { default };
    let name = explicit.filter(|name| !name.trim().is_empty()).unwrap_or_else(|| {
        if !default.is_empty() { default }
        else if !c.login_name.is_empty() { c.login_name.clone() }
        else { identity.clone() }
    });
    let mut secret = json!({ "id": identity, "uid": c.uid, "loginName": c.login_name,
        "accessToken": c.token, "loginType": "passport", "tokenTail": token_tail(&c.token),
        "name": errors::public_text(&name, &c.secrets(), 100), "source": source, "desktop": false });
    if let Some(balance) = &c.balance_token { secret["balanceToken"] = json!(balance); }
    if c.ignored_balance { secret["balanceCredentialIgnored"] = json!(true); }
    Ok(json!({ "identity": identity, "public": view(&secret)?, "secret": secret, "desktop": false }))
}

/// 只由web_login消费宿主一次性输入后调用；上游身份及令牌还会由宿主原Call见证再次比对。
pub fn from_browser(input: &Value) -> Result<Value> {
    let raw = input["token"].as_str().ok_or_else(|| errors::invalid("CatPaw 托管回调缺少令牌"))?;
    if raw.is_empty() || raw.len() > 8192 || !raw.bytes().all(|byte| matches!(byte, 0x21..=0x7e) && byte != b';') {
        return Err(errors::invalid("CatPaw 托管令牌格式无效"));
    }
    let uid = input["user"]["uid"].as_str().ok_or_else(|| errors::protocol("CatPaw 用户标识无效"))?;
    let login_name = input["user"]["loginName"].as_str().ok_or_else(|| errors::protocol("CatPaw 用户名称无效"))?;
    let payload = json!({ "accessToken": raw, "uid": uid, "loginName": login_name, "loginType": "passport" });
    let credentials = Credentials::parse(&payload)?;
    credentials.require_main()?;
    if credentials.token != raw || credentials.uid != uid || credentials.login_name != login_name {
        return Err(errors::protocol("CatPaw 托管身份格式不一致，未修改账号标识"));
    }
    let identity = credentials.identity()?;
    let name = if login_name.is_empty() { &identity } else { login_name };
    let secret = json!({ "id": identity, "uid": uid, "loginName": login_name,
        "accessToken": raw, "loginType": "passport", "tokenTail": token_tail(raw),
        "name": errors::public_text(name, &[raw], 100), "source": "browser", "desktop": false });
    let mut public = view(&secret)?;
    // 此值只表明这次宿主current-user已确认；普通手工快照仍显示format_only。
    public["validation"] = json!("current_user");
    Ok(json!({ "identity": identity, "public": public, "secret": secret, "desktop": false }))
}

fn input(params: &Value) -> Result<(Value, &'static str, Option<String>)> {
    let source = &params["source"];
    match source["kind"].as_str() {
        Some("desktop") => Err(gates::desktop()),
        Some("file_base64") => Err(errors::invalid("请选择 CatPaw 文本 JSON 登录文件，不接受二进制文件")),
        Some("file_text") => {
            let text = source["content"].as_str().ok_or_else(|| errors::invalid("CatPaw 凭据文件内容必须是文本"))?;
            Ok((util::json_object(text)?, "file", None))
        }
        Some("manual") => {
            let payload = &source["payload"];
            if payload.get("methodIndex").is_some() { return form(payload); }
            if let Some(text) = payload.as_str() { return Ok((util::json_object(text)?, "manual", None)); }
            if !payload.is_object() { return Err(errors::invalid("CatPaw 手工导入需要令牌与用户标识，或单账号 JSON")); }
            Ok((payload.clone(), "manual", None))
        }
        _ => Err(errors::invalid("请选择手工凭据或文本 JSON 快照导入")),
    }
}

fn form(payload: &Value) -> Result<(Value, &'static str, Option<String>)> {
    credentials::own_provider(payload)?;
    let index = payload["methodIndex"].as_u64().ok_or_else(|| errors::invalid("CatPaw 登录方式无效"))?;
    if index == 0 { return Err(errors::invalid("CatPaw 网页登录必须通过当前主程序托管流程完成，不能从手工账号字段提交")); }
    if index == 3 { return Err(gates::desktop()); }
    let fields = payload["inputs"].as_object().ok_or_else(|| errors::invalid("CatPaw 登录字段必须为对象"))?;
    let allowed: &[&str] = match index {
        1 => &["token", "uid", "loginName", "name"],
        2 => &["credentialJson", "name"],
        _ => return Err(errors::invalid("请选择手工凭据或文本 JSON 导入")),
    };
    if fields.iter().any(|(key, value)| !allowed.contains(&key.as_str()) || !value.is_string()) {
        return Err(errors::invalid("CatPaw 登录包含未申报字段或非文本输入"));
    }
    let name = fields.get("name").and_then(Value::as_str).map(str::trim)
        .filter(|name| !name.is_empty()).map(str::to_string);
    let value = if index == 2 {
        util::json_object(fields.get("credentialJson").and_then(Value::as_str).unwrap_or(""))?
    } else { Value::Object(fields.clone()) };
    Ok((value, "manual", name))
}
