//! 只拥有国际版账号；保留旧 intl-user-<id>/autoclaw-intl-desktop 身份。
//! 手填 enc: 与官方本机实时来源交给国际版独立授权 helper，不在 Wasm 读盘。
use super::{desktop, endpoints, profile, util};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

#[derive(Clone)]
pub struct Snapshot {
    pub record: Value,
    pub secret: Value,
    pub local: bool,
    pub stamp: String,
}

pub fn decode(value: &Value) -> Result<Value> {
    let mut value = if let Some(raw) = value.as_str() {
        serde_json::from_str(raw).map_err(|_| Error::http(400, "AutoClaw 凭据不是有效 JSON"))?
    } else {
        value.clone()
    };
    if !value.is_object() {
        return Err(Error::http(400, "AutoClaw 凭据必须是对象"));
    }
    check_owner(&value)?;
    if value["auth"].is_object() {
        check_owner(&value["auth"])?;
    }
    // 历史 auth 容器只补顶层缺失字段；保留原值供宿主 CAS，解码不直接写回。
    if let Some(auth) = value["auth"].as_object().cloned() {
        for (key, child) in auth {
            if value
                .get(&key)
                .is_none_or(|value| value.is_null() || value == "")
            {
                value[&key] = child;
            }
        }
    }
    Ok(value)
}
fn check_owner(value: &Value) -> Result<()> {
    for key in ["provider", "region"] {
        let Some(owner) = value.get(key).filter(|value| !value.is_null()) else {
            continue;
        };
        let owner = owner
            .as_str()
            .ok_or_else(|| Error::http(400, "AutoClaw 凭据地区标记格式无效"))?
            .trim();
        let allowed =
            owner.is_empty() || owner == endpoints::PROVIDER || key == "region" && owner == "intl";
        if !allowed {
            return Err(Error::http(400, "这份 AutoClaw 凭据不属于国际版"));
        }
    }
    Ok(())
}

pub fn access(secret: &Value) -> String {
    util::strip_bearer(&util::text(
        secret,
        &["token", "accessToken", "access_token"],
    ))
}
pub fn refresh_token(secret: &Value) -> String {
    util::strip_bearer(&util::text(secret, &["refreshToken", "refresh_token"]))
}
pub fn user(secret: &Value) -> String {
    let explicit = util::scalar(&secret["userId"]);
    let explicit = if explicit.is_empty() {
        util::scalar(&secret["user_id"])
    } else {
        explicit
    };
    if explicit.is_empty() {
        util::scalar(&util::claims(&access(secret))["user_id"])
    } else {
        explicit
    }
}
pub fn device(secret: &Value) -> String {
    let value = util::text(secret, &["deviceId", "device_id"]);
    if value.is_empty() {
        util::scalar(&util::claims(&access(secret))["device_id"])
    } else {
        value
    }
}
pub fn expires(secret: &Value) -> Option<i64> {
    ["expiresAt", "tokenExpiresAt"]
        .iter()
        .filter_map(|key| util::number(&secret[*key]))
        .filter(|value| *value > 0.0 && *value <= i64::MAX as f64)
        .map(|value| value as i64)
        .max()
        .or_else(|| util::jwt_expiry(&access(secret)))
}
pub fn local(secret: &Value) -> bool {
    secret["desktop"] == true
}
pub fn identity(secret: &Value) -> String {
    // 只认原记录已经保存的标识；数字 userId 与字符串同形，不由易变令牌补身份。
    let id = util::scalar(&secret["userId"]);
    let id = if id.is_empty() {
        util::scalar(&secret["user_id"])
    } else {
        id
    };
    if id.is_empty() {
        String::new()
    } else if local(secret) {
        endpoints::DESKTOP_ID.into()
    } else {
        format!("{}{id}", endpoints::ID_PREFIX)
    }
}

pub fn parse(value: &Value) -> Result<Value> {
    let mut secret = decode(value)?;
    let mut token = access(&secret);
    let mut refresh = refresh_token(&secret);
    if token.starts_with("enc:") || refresh.starts_with("enc:") {
        let pair = desktop::decrypt(&token, &refresh)?;
        token = pair.0;
        refresh = pair.1;
    }
    util::token_valid(&token, false)?;
    util::token_valid(&refresh, true)?;
    secret["token"] = json!(token);
    secret["accessToken"] = json!(token);
    secret["refreshToken"] = json!(refresh);
    let uid = user(&secret);
    let token_user = util::scalar(&util::claims(&token)["user_id"]);
    if !uid.is_empty() && !token_user.is_empty() && uid != token_user {
        return Err(
            Error::http(409, "AutoClaw 凭据中的用户标识与令牌身份不一致")
                .with_code("identity_changed"),
        );
    }
    let did = device(&secret);
    if !util::printable(&uid, 256) || !util::printable(&did, 256) {
        return Err(Error::http(400, "AutoClaw 用户或设备标识格式无效"));
    }
    secret["userId"] = json!(uid);
    secret["deviceId"] = json!(did);
    if util::text(&secret, &["email"]).is_empty() {
        let email = util::text(&secret["userInfo"], &["email"]);
        if !email.is_empty() {
            secret["email"] = json!(email);
        }
    }
    if let Some(expires) = expires(&secret) {
        secret["expiresAt"] = json!(expires);
        secret["tokenExpiresAt"] = json!(expires);
    }
    Ok(secret)
}

pub fn read() -> Result<Snapshot> {
    let record = sdk::account()?;
    if record["provider"] != endpoints::PROVIDER || sdk::account_disabled(&record) {
        return Err(Error::coded(
            "AutoClaw 当前账号已停用或不属于国际版",
            "forbidden",
        ));
    }
    let secret = decode(&record["secret"])?;
    if record["desktop"] == true {
        // 本机许可只来自宿主保存的本次导入标记，不从手工 JSON 或记录名字推导。
        let found = desktop::read(Some(&secret))?;
        return Ok(Snapshot {
            record,
            secret: found.secret,
            stamp: found.stamp,
            local: true,
        });
    }
    if local(&secret) {
        return Err(Error::coded(
            "AutoClaw 国际版本机引用缺少宿主导入标记，请通过本机导入重新确认",
            "forbidden",
        ));
    }
    Ok(Snapshot {
        record,
        secret: parse(&secret)?,
        local: false,
        stamp: String::new(),
    })
}

pub fn stamp(snapshot: &Snapshot) -> String {
    util::hash(
        &json!([
            endpoints::PROVIDER,
            snapshot.record["id"],
            snapshot.local,
            snapshot.stamp,
            user(&snapshot.secret),
            access(&snapshot.secret),
            refresh_token(&snapshot.secret)
        ])
        .to_string(),
    )
}
pub fn same(a: &Snapshot, b: &Snapshot) -> bool {
    a.record["id"] == b.record["id"]
        && a.record["provider"] == b.record["provider"]
        && a.record["identity"] == b.record["identity"]
        && a.record["desktop"] == b.record["desktop"]
        && a.record["secret"] == b.record["secret"]
        && stamp(a) == stamp(b)
}

pub fn view(value: &Value) -> Result<Value> {
    let secret = decode(value)?;
    let token = access(&secret);
    let refresh = refresh_token(&secret);
    let uid = user(&secret);
    let email = util::text(&secret, &["email"]);
    let name = util::text(&secret, &["name", "email"]);
    let name = if name.is_empty() {
        if uid.is_empty() {
            "AutoClaw 国际版账号".into()
        } else {
            format!("AutoClaw 国际版 · {uid}")
        }
    } else {
        name
    };
    if [&name, &uid, &email].iter().any(|public| {
        [&token, &refresh]
            .iter()
            .any(|token| !token.is_empty() && public.contains(token.as_str()))
    }) {
        return Err(Error::http(400, "AutoClaw 公开账号字段不能包含完整凭据"));
    }
    if [&name, &uid, &email]
        .iter()
        .any(|field| field.len() > 1024 || field.chars().any(char::is_control))
    {
        return Err(Error::http(400, "AutoClaw 公开账号字段格式无效"));
    }
    let tail: String = if token.is_empty() {
        util::text(&secret, &["tokenTail"])
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    } else if token.chars().count() <= 4 {
        String::new()
    } else {
        token
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    };
    let can_refresh = !refresh.is_empty() || local(&secret) && secret["hasRefreshToken"] == true;
    let mut public = json!({ "name": name, "userId": uid, "tokenTail": tail,
        "hasSession": !token.is_empty() || local(&secret) && secret["hasSession"] == true,
        "desktop": local(&secret), "region": endpoints::PROVIDER, "edition": "国际版",
        "canRefresh": can_refresh && !local(&secret), "canSyncCredentials": local(&secret), "hasRefreshToken": can_refresh });
    if !email.is_empty() {
        public["email"] = json!(email);
    }
    if let Some(expires) = expires(&secret) {
        public["expiresAt"] = json!(expires);
        public["tokenExpiresAt"] = json!(expires);
    }
    Ok(public)
}

pub fn normalized(secret: Value) -> Result<Value> {
    Ok(
        json!({ "identity": identity(&secret), "desktop": local(&secret), "public": view(&secret)?, "secret": secret }),
    )
}

pub fn normalize(params: &Value) -> Result<Value> {
    let source = &params["source"];
    if source["kind"] == "manual" && source["payload"]["methodIndex"] == 5 {
        let legacy = sdk::legacy_credentials::take()?;
        return normalize(&json!({ "source": { "kind": "manual", "payload": legacy }, "name": params["name"] }));
    }
    if source["kind"] == "manual"
        && source["payload"].get("methodIndex").is_some()
        && !matches!(source["payload"]["methodIndex"].as_u64(), Some(2 | 3))
    {
        return Err(Error::http(
            400,
            "AutoClaw 国际版此方式必须从官方授权或本机导入流程发起",
        ));
    }
    if source["kind"] == "desktop" {
        let found = desktop::read(None)?;
        let mut reference = desktop::reference(&found.secret)?;
        let name = util::text(params, &["name"]);
        if !name.is_empty() {
            reference["name"] = json!(name);
        }
        return normalized(reference);
    }
    let payload = match source["kind"].as_str() {
        Some("manual") => {
            let value = &source["payload"];
            let value = if value["inputs"].is_object() {
                &value["inputs"]
            } else {
                value
            };
            if let Some(raw) = value["credentialJson"]
                .as_str()
                .filter(|raw| !raw.trim().is_empty())
            {
                serde_json::from_str(raw)
                    .map_err(|_| Error::http(400, "AutoClaw 凭据 JSON 格式无效"))?
            } else {
                value.clone()
            }
        }
        Some("file_text") => serde_json::from_str(source["content"].as_str().unwrap_or(""))
            .map_err(|_| Error::http(400, "请选择 AutoClaw auth.json 或填写凭据 JSON"))?,
        _ => {
            return Err(Error::http(
                400,
                "AutoClaw 国际版请使用官方授权、手动凭据、JSON 文本或受控本机导入",
            ))
        }
    };
    let payload = decode(&payload)?;
    if local(&payload) {
        return Err(Error::http(
            400,
            "本机实时引用请使用本机导入入口，不从手动凭据启用读盘",
        ));
    }
    let mut secret = parse(&payload)?;
    if device(&secret).is_empty() {
        secret["deviceId"] = json!(util::random_hex(32)?);
    }
    // 手工导入仍核对已登录的只读资料接口；缺少可选邮箱不等于鉴权失败。
    let profile = profile::fetch(&secret)?;
    let uid = util::text(&profile, &["userId"]);
    if !uid.is_empty() {
        let previous = user(&secret);
        if !previous.is_empty() && previous != uid {
            return Err(
                Error::http(409, "AutoClaw 国际版资料与凭据身份不一致，未添加账号")
                    .with_code("identity_changed"),
            );
        }
        secret["userId"] = json!(uid);
    }
    if user(&secret).is_empty() {
        return Err(Error::http(
            502,
            "AutoClaw 国际版凭据与用户资料均未给出账号身份，未添加空账号",
        ));
    }
    let email = util::text(&profile, &["email"]);
    if !email.is_empty() {
        secret["email"] = json!(email);
    }
    let name = util::text(params, &["name"]);
    if !name.is_empty() {
        secret["name"] = json!(name);
    }
    normalized(project(&secret))
}

pub fn project(secret: &Value) -> Value {
    let mut output = json!({ "provider": endpoints::PROVIDER, "region": "intl",
        "token": access(secret), "accessToken": access(secret), "refreshToken": refresh_token(secret),
        "deviceId": device(secret), "userId": user(secret), "desktop": false });
    for field in ["email", "name", "phoneTail"] {
        let text = util::text(secret, &[field]);
        if !text.is_empty() {
            output[field] = json!(text);
        }
    }
    if let Some(expires) = expires(secret) {
        output["expiresAt"] = json!(expires);
        output["tokenExpiresAt"] = json!(expires);
    }
    output
}

pub fn save(current: &Snapshot, next: Value) -> Result<bool> {
    if current.local {
        return Err(Error::coded(
            "AutoClaw 本机来源不能写回账号令牌",
            "forbidden",
        ));
    }
    let mut public = view(&next)?;
    if let Some(name) = current.record["public"]["name"].as_str() {
        public["name"] = json!(name);
    }
    Ok(sdk::host(
        "account.update",
        json!({ "expect": current.record["secret"], "secret": next, "public": public }),
    )?["updated"]
        == true)
}
