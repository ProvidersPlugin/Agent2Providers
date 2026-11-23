//! 显式本机导入是一次性快照，与原生一致；后续不读本机、不使用环境变量旁路。
//! uid/identity、组织和授权字段保持原契约，公开视图仅令牌尾号。
use super::{desktop, endpoints, util};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub fn identity(secret: &Value) -> String {
    let uid = util::text(secret, &["uid"]);
    if uid.is_empty() {
        String::new()
    } else {
        format!("user-{uid}")
    }
}
fn nested(payload: &Value, group: &str, keys: &[&str]) -> String {
    let inside = util::text(&payload[group], keys);
    if inside.is_empty() {
        util::text(payload, keys)
    } else {
        inside
    }
}
fn text_field(value: &str, max: usize, label: &str) -> Result<()> {
    if value.len() > max || value.chars().any(char::is_control) {
        return Err(Error::http(400, format!("WorkBuddy{label}字段无效或过长")));
    }
    Ok(())
}
pub fn parse(payload: &Value, name: Option<&str>) -> Result<Value> {
    if !payload.is_object() || payload.to_string().len() > endpoints::MAX_CREDENTIALS {
        return Err(Error::http(
            400,
            "WorkBuddy凭据必须为不超过256KiB的JSON对象",
        ));
    }
    for key in ["edition", "realm", "region"] {
        let edition = util::text(payload, &[key]);
        if !edition.is_empty()
            && !["cn", "china", "mainland", "workbuddy", "zh", "cn-intl"]
                .contains(&edition.to_ascii_lowercase().as_str())
        {
            return Err(Error::http(
                400,
                "该登录态不是WorkBuddy国内版，请使用对应地区插件",
            ));
        }
    }
    // 平台/前缀只做地区识别，不能当协议校验：wb2api 等第三方导出会带自己的
    // 通道名（如 "CLI"），官方 .info 也不写这两个字段。它们不参与请求，解析后
    // 统一按国内版规范值写回，所以这里只拦明确属于国际版的标记。
    if payload
        .get("platform")
        .and_then(|value| value.as_str())
        .is_some_and(|platform| {
            matches!(
                platform.trim().to_ascii_lowercase().as_str(),
                "workbuddy-ai" | "workbuddyai" | "workbuddy.ai"
            )
        })
    {
        return Err(Error::http(400, "这是WorkBuddy国际版登录态，请使用国际版插件"));
    }
    for key in [
        "accessToken",
        "token",
        "access_token",
        "refreshToken",
        "refresh_token",
    ] {
        if payload["auth"]
            .get(key)
            .or_else(|| payload.get(key))
            .is_some_and(|value| value.is_object() && value.get("$wbEncrypted").is_some())
        {
            return Err(Error::http(
                400,
                "WorkBuddy 客户端已加密保存此登录态，无法直接导入；请改用网页登录，不需要手动复制令牌",
            ));
        }
    }
    let token = util::strip(&nested(
        payload,
        "auth",
        &["accessToken", "token", "access_token"],
    ));
    let refresh = util::strip(&nested(payload, "auth", &["refreshToken", "refresh_token"]));
    if token.is_empty()
        || token.len() > 32_768
        || refresh.len() > 32_768
        || token.chars().any(char::is_whitespace)
        || refresh.chars().any(char::is_whitespace)
    {
        return Err(Error::http(
            400,
            "WorkBuddy登录态缺少有效accessToken或令牌格式无效",
        ));
    }
    let uid = nested(payload, "account", &["uid"]);
    text_field(&uid, 256, "账号身份")?;
    if uid.is_empty() {
        return Err(Error::http(400, "WorkBuddy登录态缺少uid，无法标识账号"));
    }
    if util::claims(&token)
        .map(|claims| util::text(&claims, &["sub", "userId", "uid"]))
        .is_some_and(|claimed| !claimed.is_empty() && claimed != uid)
    {
        return Err(Error::http(
            400,
            "WorkBuddy令牌身份与账号uid不一致，未导入另一账号的凭据",
        ));
    }
    let mut secret = json!({ "uid": uid, "accessToken": token, "endpoint": endpoints::endpoint(payload)?,
        "edition": "cn", "prefixPath": "/plugin", "platform": "workbuddy" });
    if !refresh.is_empty() {
        secret["refreshToken"] = json!(refresh);
    }
    for key in [
        "nickname",
        "type",
        "enterpriseId",
        "enterpriseName",
        "departmentFullName",
        "oneidAccountId",
        "idp",
    ] {
        let value = nested(payload, "account", &[key]);
        text_field(&value, 256, key)?;
        if !value.is_empty() {
            secret[key] = json!(value);
        }
    }
    for key in ["domain", "tokenType"] {
        let value = nested(payload, "auth", &[key]);
        text_field(&value, 256, key)?;
        if key == "domain" && !value.is_empty() {
            let domain = url::Url::parse(&value)
                .ok()
                .and_then(|url| url.host_str().map(str::to_string))
                .unwrap_or_else(|| value.to_ascii_lowercase());
            if ["workbuddy.ai", "codebuddy.ai"]
                .iter()
                .any(|intl| domain == *intl || domain.ends_with(&format!(".{intl}")))
            {
                return Err(Error::http(
                    400,
                    "这是WorkBuddy国际版登录态，请使用国际版插件",
                ));
            }
        }
        if !value.is_empty() {
            secret[key] = json!(value);
        }
    }
    for (key, alias) in [
        ("expiresAt", "expires_at"),
        ("refreshExpiresAt", "refresh_expires_at"),
    ] {
        let value = [
            payload["auth"].get(key),
            payload["auth"].get(alias),
            payload.get(key),
            payload.get(alias),
        ]
        .into_iter()
        .flatten()
        .find_map(|value| util::integer(value).filter(|at| *at > 0));
        let token = if key == "expiresAt" { &token } else { &refresh };
        if let Some(at) = value.or_else(|| {
            util::claims(token)
                .and_then(|claims| util::integer(&claims["exp"]))
                .filter(|seconds| *seconds > 0)
                .and_then(|seconds| seconds.checked_mul(1000))
        }) {
            secret[key] = json!(at);
        }
    }
    let nickname = util::text(&secret, &["nickname"]);
    let name = name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| (!nickname.is_empty()).then_some(nickname))
        .unwrap_or_else(|| format!("账号 {}", uid.chars().take(8).collect::<String>()));
    text_field(&name, 400, "备注")?;
    secret["name"] = json!(name.chars().take(100).collect::<String>());
    secret["tokenTail"] = json!(util::tail(&token));
    Ok(secret)
}
pub fn view(secret: &Value) -> Value {
    let mut value = json!({ "uid": util::text(secret, &["uid"]), "name": util::text(secret, &["name", "nickname"]),
        "nickname": util::text(secret, &["nickname"]), "type": util::text(secret, &["type"]),
        "enterpriseId": util::text(secret, &["enterpriseId"]), "enterpriseName": util::text(secret, &["enterpriseName"]),
        "tokenTail": util::tail(&util::text(secret, &["accessToken"])), "hasRefreshToken": !util::text(secret, &["refreshToken"]).is_empty(),
        "edition": "cn", "editionLabel": "国内版", "endpoint": secret["endpoint"], "prefixPath": "/plugin",
        "available": !util::text(secret, &["accessToken"]).is_empty(), "expiresAt": secret["expiresAt"], "refreshTokenExpiresAt": secret["refreshExpiresAt"] });
    if value["name"] == "" {
        value["name"] = json!("WorkBuddy国内账号");
    }
    value
}
pub fn bundle(secret: Value) -> Result<Value> {
    let public = view(&secret);
    let display = public.to_string();
    if ["accessToken", "refreshToken"]
        .iter()
        .filter_map(|key| secret[*key].as_str())
        .filter(|v| v.len() > 6)
        .any(|v| display.contains(v))
    {
        return Err(Error::http(
            400,
            "公开账号信息含有令牌，请修改备注或组织名称",
        ));
    }
    Ok(
        json!({ "identity": identity(&secret), "public": public, "secret": secret, "desktop": false }),
    )
}
pub fn normalize(params: &Value) -> Result<Value> {
    let source = &params["source"];
    let payload = match source["kind"].as_str() {
        Some("desktop") => desktop::read()?,
        Some("manual") => {
            let payload = &source["payload"];
            if payload.get("methodIndex").is_none() {
                payload.clone()
            } else if payload["methodIndex"] == 1 {
                let text = payload["inputs"]["credentialJson"]
                    .as_str()
                    .ok_or_else(|| Error::http(400, "请粘贴登录态JSON"))?;
                serde_json::from_str(text).map_err(|_| Error::http(400, "登录态不是有效JSON"))?
            } else {
                return Err(Error::http(400, "此方式不是手动JSON入口"));
            }
        }
        Some("file_text") => serde_json::from_str(source["content"].as_str().unwrap_or(""))
            .map_err(|_| Error::http(400, "请选择官方客户端的文本.info文件"))?,
        _ => return Err(Error::http(400, "WorkBuddy不支持该凭据来源")),
    };
    bundle(parse(
        &payload,
        params["name"].as_str().or_else(|| payload["name"].as_str()),
    )?)
}
pub fn record() -> Result<Value> {
    sdk::account()
}
pub fn save(record: &Value, secret: &Value) -> Result<bool> {
    let validated = parse(secret, secret["name"].as_str())?;
    let mut public = view(&validated);
    if let Some(name) = record["public"]["name"].as_str().filter(|s| !s.is_empty()) {
        public["name"] = json!(name);
    }
    Ok(sdk::host(
        "account.update",
        json!({ "expect": record["secret"], "secret": secret, "public": public }),
    )?["updated"]
        == true)
}
pub fn authority(secret: &Value) -> Value {
    let mut value = json!({ "region": "cn", "endpoint": secret["endpoint"] });
    for key in [
        "uid",
        "type",
        "enterpriseId",
        "oneidAccountId",
        "departmentFullName",
        "idp",
        "domain",
        "tokenType",
    ] {
        if let Some(field) = secret.get(key) {
            value[key] = field.clone();
        }
    }
    value
}
pub fn fingerprint(secret: &Value) -> String {
    util::hash(&authority(secret).to_string())
}
pub fn same_auth(a: &Value, b: &Value) -> bool {
    authority(a) == authority(b)
        && a["accessToken"] == b["accessToken"]
        && a["refreshToken"] == b["refreshToken"]
        && a["prefixPath"] == b["prefixPath"]
        && a["platform"] == b["platform"]
}
pub fn personal(secret: &Value) -> bool {
    util::text(secret, &["enterpriseId"]).is_empty()
}
