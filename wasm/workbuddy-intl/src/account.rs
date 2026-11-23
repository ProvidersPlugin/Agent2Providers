//! 国际账号保持user-<uid>身份；官方.info嵌套/扁平JSON均可，桌面导入后是独立快照。
use crate::{desktop, endpoints, util};
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
fn field(text: &str, limit: usize) -> Result<()> {
    if text.len() > limit || text.chars().any(char::is_control) {
        return Err(Error::http(400, "WorkBuddy国际版凭据字段无效或过长"));
    }
    Ok(())
}
pub fn parse(payload: &Value, name: Option<&str>) -> Result<Value> {
    if !payload.is_object() || payload.to_string().len() > endpoints::MAX_CREDENTIALS {
        return Err(Error::http(400, "国际版凭据必须为不超过256KiB的JSON对象"));
    }
    let edition = util::text(payload, &["edition"]);
    if !edition.is_empty()
        && ![
            "intl",
            "international",
            "global",
            "ai",
            "workbuddy-ai",
            "workbuddyai",
            "en",
        ]
        .contains(&edition.to_ascii_lowercase().as_str())
    {
        return Err(Error::http(
            400,
            "该凭据不是WorkBuddy国际版，请使用对应地区插件",
        ));
    }
    // 可选地区别名只拒绝明确的国内来源，不把其它导出工具的附带区域字段当登录档位。
    for key in ["realm", "region"] {
        if ["cn", "china", "mainland", "workbuddy", "zh", "cn-intl"]
            .contains(&util::text(payload, &[key]).to_ascii_lowercase().as_str())
        {
            return Err(Error::http(400, "该凭据是国内版，请使用对应地区插件"));
        }
    }
    // 平台/前缀只做地区识别，不能当协议校验：第三方导出会带自己的通道名
    // （如 "CLI"），官方 .info 也不写这两个字段。它们不参与请求，解析后统一按
    // 国际版规范值写回，所以这里只拦明确属于国内版的标记。
    if payload
        .get("platform")
        .and_then(|value| value.as_str())
        .is_some_and(|platform| platform.trim().eq_ignore_ascii_case("workbuddy"))
    {
        return Err(Error::http(400, "该凭据是国内版，请使用对应地区插件"));
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
                "国际客户端已加密此登录态，请改用官方网页登录，不需要复制令牌",
            ));
        }
    }
    let token = util::strip(&nested(
        payload,
        "auth",
        &["accessToken", "token", "access_token"],
    ));
    let refresh = util::strip(&nested(payload, "auth", &["refreshToken", "refresh_token"]));
    if token.len() < 8
        || token.len() > 32_768
        || refresh.len() > 32_768
        || token.chars().any(|c| c.is_whitespace() || c.is_control())
        || refresh.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(Error::http(
            400,
            "国际版登录态缺少有效accessToken或令牌格式无效",
        ));
    }
    let uid = nested(payload, "account", &["uid"]);
    field(&uid, 256)?;
    if uid.is_empty() {
        return Err(Error::http(400, "国际版登录态缺少uid，无法标识账号"));
    }
    if util::claims(&token)
        .map(|claims| util::text(&claims, &["sub", "userId", "uid"]))
        .is_some_and(|claimed| !claimed.is_empty() && claimed != uid)
    {
        return Err(Error::http(
            400,
            "国际令牌身份与账号uid不一致，未导入另一账号的凭据",
        ));
    }
    let mut secret = json!({ "uid": uid, "accessToken": token, "endpoint": endpoints::endpoint(payload)?,
        "edition": "intl", "prefixPath": "/plugin", "platform": endpoints::PLATFORM });
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
        field(&value, 256)?;
        if !value.is_empty() {
            secret[key] = json!(value);
        }
    }
    for key in ["domain", "tokenType"] {
        let value = nested(payload, "auth", &[key]);
        field(&value, 256)?;
        if key == "domain" && !value.is_empty() {
            let domain = url::Url::parse(&value)
                .ok()
                .and_then(|url| url.host_str().map(str::to_string))
                .unwrap_or_else(|| value.to_ascii_lowercase());
            if ["workbuddy.cn", "codebuddy.cn", "copilot.tencent.com"]
                .iter()
                .any(|domestic| domain == *domestic || domain.ends_with(&format!(".{domestic}")))
            {
                return Err(Error::http(
                    400,
                    "该授权域属于国内版，未导入另一地区的账号凭据",
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
        let at = [
            payload["auth"].get(key),
            payload["auth"].get(alias),
            payload.get(key),
            payload.get(alias),
        ]
        .into_iter()
        .flatten()
        .find_map(|value| util::integer(value).filter(|at| *at > 0));
        let credential = if key == "expiresAt" { &token } else { &refresh };
        if let Some(at) = at.or_else(|| {
            util::claims(credential)
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
    field(&name, 400)?;
    secret["name"] = json!(name.chars().take(100).collect::<String>());
    secret["tokenTail"] = json!(util::tail(&token));
    Ok(secret)
}
pub fn view(secret: &Value) -> Value {
    json!({ "uid": util::text(secret, &["uid"]), "name": util::text(secret, &["name", "nickname"]),
        "nickname": util::text(secret, &["nickname"]), "type": util::text(secret, &["type"]),
        "enterpriseId": util::text(secret, &["enterpriseId"]), "enterpriseName": util::text(secret, &["enterpriseName"]),
        "tokenTail": secret["accessToken"].as_str().filter(|token| token.len() >= 8).map(util::tail).unwrap_or_default(), "hasRefreshToken": !util::text(secret, &["refreshToken"]).is_empty(),
        "edition": "intl", "editionLabel": "国际版", "endpoint": endpoints::ORIGIN, "prefixPath": "/plugin",
        "available": !util::text(secret, &["accessToken"]).is_empty(),
        "canRefresh": !util::text(secret, &["refreshToken"]).is_empty()
            && util::integer(&secret["refreshExpiresAt"]).is_none_or(|at| at <= 0 || at > sdk::now()),
        "expiresAt": util::integer(&secret["expiresAt"]).filter(|at| *at >= 0),
        "refreshTokenExpiresAt": util::integer(&secret["refreshExpiresAt"]).filter(|at| *at >= 0) })
}
fn validate_view(public: &Value, secret: &Value) -> Result<()> {
    let text = public.to_string();
    if ["accessToken", "refreshToken"]
        .iter()
        .filter_map(|k| secret[*k].as_str())
        .filter(|v| v.len() > 6)
        .any(|v| text.contains(v))
    {
        return Err(Error::http(400, "公开账号信息包含凭据，请修改备注或资料"));
    }
    Ok(())
}
pub fn bundle(secret: Value) -> Result<Value> {
    let public = view(&secret);
    validate_view(&public, &secret)?;
    Ok(
        json!({ "identity": identity(&secret), "secret": secret, "public": public, "desktop": false }),
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
                    .ok_or_else(|| Error::http(400, "请粘贴国际版登录态JSON"))?;
                serde_json::from_str(text).map_err(|_| Error::http(400, "登录态不是有效JSON"))?
            } else {
                return Err(Error::http(400, "此方式不是手动JSON入口"));
            }
        }
        Some("file_text") => serde_json::from_str(source["content"].as_str().unwrap_or(""))
            .map_err(|_| Error::http(400, "请选择国际客户端的文本.info文件"))?,
        _ => return Err(Error::http(400, "国际版不支持该凭据来源")),
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
    validate_view(&public, secret)?;
    Ok(sdk::host(
        "account.update",
        json!({ "expect": record["secret"], "secret": secret, "public": public }),
    )?["updated"]
        == true)
}
pub fn canonical_domain(value: &str) -> &str {
    if value.eq_ignore_ascii_case("www.codebuddy.ai")
        || value.eq_ignore_ascii_case("www.workbuddy.ai")
    {
        "www.workbuddy.ai"
    } else {
        value
    }
}
pub fn authority(secret: &Value) -> Value {
    let mut value = json!({ "region": "intl", "endpoint": endpoints::ORIGIN });
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
