//! 普通账号identity保持user-<平台uid>；实时桌面引用保持raccoon-desktop且不落token。
//! 只在明确source.kind=desktop时读取helper，手动载荷不能把自己声明成桌面引用。

use super::{credentials::Credential, desktop, endpoints, iam, jwt, util};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub fn identity(secret: &Value) -> String {
    if secret["desktop"] == true {
        return endpoints::DESKTOP_ID.into();
    }
    let user = util::text(secret, &["userId"]);
    if user.is_empty() {
        String::new()
    } else {
        format!("user-{user}")
    }
}
pub fn view(secret: &Value) -> Value {
    let desktop = secret["desktop"] == true;
    let token = jwt::strip(&util::text(
        secret,
        &["accessToken", "access_token", "token", "auth_token"],
    ));
    let user = util::text(secret, &["userId"]);
    let name = util::text(secret, &["name", "nickname"]);
    let name = if name.is_empty() {
        if user.is_empty() {
            "小浣熊账号".into()
        } else {
            format!("账号 {user}")
        }
    } else {
        name
    };
    let refreshable = if desktop {
        secret["canRefresh"] == true && secret["desktopReadOnly"] != true
    } else {
        !jwt::iam(&token) && !util::text(secret, &["refreshToken", "refresh_token"]).is_empty()
    };
    let has_refresh = if desktop {
        secret["canRefresh"] == true
    } else {
        refreshable
    };
    let has_session = if desktop {
        secret["hasSession"]
            .as_bool()
            .unwrap_or_else(|| !util::text(secret, &["tokenTail"]).is_empty())
    } else {
        !token.is_empty()
    };
    let mut view = json!({ "name": name, "uid": user, "userId": user,
        "tokenTail": if desktop { util::tail(secret["tokenTail"].as_str().unwrap_or("")) } else { util::tail(&token) },
        "hasSession": has_session, "canRefresh": refreshable,
        "hasRefreshToken": has_refresh, "canSyncCredentials": desktop });
    let expires = jwt::expiry(&token)
        .or_else(|| util::integer(&secret["expiresAt"]))
        .or_else(|| util::integer(&secret["tokenExpiresAt"]));
    if let Some(at) = expires {
        view["expiresAt"] = json!(at);
    }
    for (target, aliases) in [
        ("officeIdentity", &["officeIdentity", "office_identity"][..]),
        ("officeOrgName", &["officeOrgName", "office_org_name"][..]),
        ("officeOrgRole", &["officeOrgRole", "office_org_role"][..]),
    ] {
        let value = util::text(secret, aliases);
        if !value.is_empty() {
            view[target] = json!(value);
        }
    }
    view
}

fn canonical_payload(payload: &Value) -> Value {
    if !payload.is_object() {
        return payload.clone();
    }
    let mut result = json!({});
    for aliases in [
        &["accessToken", "access_token", "token", "auth_token"][..],
        &["refreshToken", "refresh_token"][..],
        &["expiresAt", "tokenExpiresAt"][..],
        &["userId"][..],
        &["officeIdentity", "office_identity"][..],
        &["officeOrgName", "office_org_name"][..],
        &["officeOrgRole", "office_org_role"][..],
        &["name", "nickname"][..],
    ] {
        let present = |value: &Value| {
            !value.is_null() && value.as_str().is_none_or(|text| !text.trim().is_empty())
        };
        let value = aliases
            .iter()
            .find_map(|key| payload.get(*key).filter(|value| present(value)))
            .or_else(|| {
                aliases
                    .iter()
                    .find_map(|key| payload["auth"].get(*key).filter(|value| present(value)))
            });
        if let Some(value) = value {
            result[aliases[0]] = value.clone();
        }
    }
    result
}

pub fn normalized(payload: &Value, name: Option<&str>) -> Result<Value> {
    if payload.to_string().len() > endpoints::MAX_CREDENTIALS {
        return Err(Error::http(400, "小浣熊凭据超过256KiB限制"));
    }
    let payload = canonical_payload(payload);
    let payload = &payload;
    let mut credential = Credential::parse(payload)?;
    let claims = jwt::claims(&credential.token).unwrap_or(Value::Null);
    let profile = if jwt::iam(&credential.token) {
        Some(iam::profile(&credential.token)?)
    } else {
        None
    };
    credential.user = if let Some(profile) = &profile {
        util::text(profile, &["id"])
    } else {
        let user = jwt::uid(&claims);
        if user.is_empty() {
            "unknown".into()
        } else {
            user
        }
    };
    if credential.user.len() > 256 || credential.user.contains('\0') {
        return Err(Error::http(400, "小浣熊平台用户ID无效"));
    }
    if util::text(payload, &["officeIdentity", "office_identity"]).is_empty() {
        if let Some(profile) = &profile {
            let org = util::text(profile, &["officeIdentity", "office_identity"]);
            if org != "personal" {
                credential.org = org;
            }
        }
    }
    let mut secret = credential.secret();
    for (target, aliases) in [
        ("officeOrgName", &["officeOrgName", "office_org_name"][..]),
        ("officeOrgRole", &["officeOrgRole", "office_org_role"][..]),
    ] {
        let mut value = util::text(payload, aliases);
        if value.is_empty() {
            value = profile
                .as_ref()
                .map(|profile| util::text(profile, aliases))
                .unwrap_or_default();
        }
        if !value.is_empty() {
            secret[target] = json!(value);
        }
    }
    let display = name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| {
            profile
                .as_ref()
                .map(|profile| util::text(profile, &["name"]))
                .filter(|name| !name.is_empty())
        })
        .unwrap_or_else(|| util::text(&claims, &["name"]));
    secret["name"] = json!(if display.is_empty() {
        format!("账号 {}", credential.user)
    } else {
        display
    });
    let _ = Credential::parse(&secret)?;
    bundle(secret, false)
}

pub fn bundle(secret: Value, desktop: bool) -> Result<Value> {
    let public = view(&secret);
    let secrets = [
        util::text(&secret, &["accessToken", "access_token", "token"]),
        util::text(&secret, &["refreshToken", "refresh_token"]),
    ];
    let text = public.to_string();
    if secrets
        .iter()
        .any(|secret| secret.len() > 6 && text.contains(secret))
    {
        return Err(Error::http(
            400,
            "公开账号信息包含令牌，请修改备注或组织信息",
        ));
    }
    Ok(
        json!({ "identity": identity(&secret), "secret": secret, "public": public, "desktop": desktop }),
    )
}

pub fn normalize(params: &Value) -> Result<Value> {
    let source = &params["source"];
    if source["kind"] == "manual" && source["payload"]["methodIndex"] == 4 {
        let legacy = sdk::legacy_credentials::take()?;
        return normalized(&legacy, params["name"].as_str());
    }
    if source["kind"] == "desktop" {
        return desktop::normalize(params["name"].as_str());
    }
    let payload = match source["kind"].as_str() {
        Some("manual") => {
            let value = &source["payload"];
            match value["methodIndex"].as_u64() {
                None => value.clone(),
                Some(index @ (1 | 2)) => {
                    let inputs = value["inputs"]
                        .as_object()
                        .filter(|inputs| {
                            inputs.iter().all(|(key, value)| {
                                value.is_string()
                                    && if index == 1 {
                                        matches!(
                                            key.as_str(),
                                            "accessToken" | "refreshToken" | "officeIdentity"
                                        )
                                    } else {
                                        key == "credentialJson"
                                    }
                            })
                        })
                        .ok_or_else(|| Error::http(400, "小浣熊手动凭据字段无效"))?;
                    if index == 1 {
                        Value::Object(inputs.clone())
                    } else {
                        serde_json::from_str(
                            inputs
                                .get("credentialJson")
                                .and_then(Value::as_str)
                                .unwrap_or(""),
                        )
                        .map_err(|_| Error::http(400, "小浣熊登录态不是合法JSON"))?
                    }
                }
                _ => return Err(Error::http(400, "此登录方式不能作为手动凭据提交")),
            }
        }
        Some("file_text") => serde_json::from_str(source["content"].as_str().unwrap_or(""))
            .map_err(|_| Error::http(400, "凭据文件不是合法JSON，请选择auth.json"))?,
        _ => return Err(Error::http(400, "小浣熊不支持该凭据来源")),
    };
    if payload["desktop"] == true
        || payload["source"] == "desktop"
        || payload["auth"]["desktop"] == true
        || payload["auth"]["source"] == "desktop"
    {
        return Err(Error::http(
            400,
            "本机引用必须显式选择桌面导入，不能通过手动JSON启用",
        ));
    }
    normalized(
        &payload,
        params["name"].as_str().or_else(|| payload["name"].as_str()),
    )
}

pub fn record() -> Result<Value> {
    sdk::account()
}
pub fn save(record: &Value, secret: &Value) -> Result<bool> {
    let mut public = view(secret);
    if secret["desktop"] == true && secret["sourceRevision"].is_string()
        && secret["expiresAt"].as_i64().is_none_or(|at| at > sdk::now())
        && (record["secret"]["sourceRevision"].is_string()
                && secret["sourceRevision"] != record["secret"]["sourceRevision"]
            || secret["tokenTail"] != record["secret"]["tokenTail"]
            || secret["expiresAt"] != record["secret"]["expiresAt"]) {
        public["needsRelogin"] = json!(false);
        public["needsReloginReason"] = json!("");
    }
    if let Some(name) = record["public"]["name"]
        .as_str()
        .filter(|name| !name.is_empty())
    {
        public["name"] = json!(name);
    }
    let result = sdk::host(
        "account.update",
        json!({ "expect": record["secret"], "secret": secret, "public": public }),
    )?;
    Ok(result["updated"] == true)
}
