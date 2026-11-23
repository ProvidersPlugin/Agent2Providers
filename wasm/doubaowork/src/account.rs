//! 保持原生业务身份；主键由 accounts.identity.v1 的宿主分配或保留。
//! 普通调用冻结原记录并在线核对身份，不刷新/改写 Cookie，不使用本机实时来源。

use agent2api_wasm_sdk::{self as sdk, json, Result, Value};

use crate::{
    credentials::{self, Credentials},
    endpoints::{self, ReadEndpoint},
    errors, gates, login_methods, network, util,
};

fn user(secret: &Value) -> Result<String> {
    util::identifier(&secret["userId"]).ok_or_else(errors::identity)
}
fn enterprise(secret: &Value) -> Result<String> {
    match secret
        .get("enterpriseType")
        .filter(|value| !value.is_null())
    {
        Some(value) => util::identifier(value).ok_or_else(errors::identity),
        None => Ok("0".into()),
    }
}
pub fn identity(secret: &Value) -> Result<String> {
    let secret = credentials::payload(secret)?;
    let identity = format!("doubaowork-{}-{}", user(&secret)?, enterprise(&secret)?);
    if identity.len() > 512
        || ["identity", "pluginIdentity"].iter().any(|key| {
            secret
                .get(*key)
                .is_some_and(|value| !value.is_null() && value.as_str() != Some(identity.as_str()))
        })
    {
        return Err(errors::identity());
    }
    Ok(identity)
}

pub struct Profile {
    pub user: String,
    pub enterprise: String,
    pub name: String,
    pub email: Value,
    pub avatar: Value,
}

fn identify(credentials: &Credentials) -> Result<Profile> {
    let value = network::read(credentials, ReadEndpoint::Identity, None)?;
    if value
        .get("message")
        .is_some_and(|value| !value.is_null() && value != "success")
    {
        return Err(errors::upstream(401, &Value::Null, ReadEndpoint::Identity));
    }
    let data = util::object_data(&value, "豆包工作身份核对未返回有效账号对象")?;
    let user = util::identifier(&data["user_id_str"]).ok_or_else(errors::identity)?;
    let enterprise = match data
        .get("passport_enterprise_user_type")
        .filter(|value| !value.is_null())
    {
        Some(value) => util::identifier(value).ok_or_else(errors::identity)?,
        None => "0".into(),
    };
    let mut identity_material = vec![credentials.cookie_header.clone()];
    for row in credentials.cookies.as_array().into_iter().flatten() {
        if matches!(row["name"].as_str(), Some("sessionid" | "sessionid_ss")) {
            if let Some(value) = row["value"].as_str().filter(|value| !value.is_empty()) {
                identity_material.push(value.into());
            }
        }
    }
    if util::protected(&user, &identity_material)
        || util::protected(&enterprise, &identity_material)
    {
        return Err(errors::identity());
    }
    let name = util::safe_text(util::text(&data, &["name"]), &credentials.protected, 100);
    let email = data["email"]
        .as_str()
        .map(|email| json!(util::safe_text(email, &credentials.protected, 254)))
        .unwrap_or(Value::Null);
    let avatar = util::safe_url(util::text(&data, &["avatar_url"]), &credentials.protected);
    Ok(Profile {
        user,
        enterprise,
        name,
        email,
        avatar,
    })
}

pub fn view(input: &Value) -> Result<Value> {
    let secret = credentials::payload(input)?;
    let identity = identity(&secret)?;
    if secret["source"] == "desktop" && !credentials::snapshot_present(&secret) {
        return Ok(
            json!({ "name": "豆包工作旧版实时关联（请重新导入）", "nickname": "豆包工作旧版实时关联",
            "userId": user(&secret)?, "email": null, "avatar": null, "tokenTail": "",
            "hasCredentials": false, "desktop": true, "available": false,
            "reason": gates::LEGACY, "migrationStage": "complete", "availabilityMessage": gates::SUMMARY }),
        );
    }
    let credentials = Credentials::parse(&secret, util::now()?)?;
    if util::protected(&identity, &credentials.protected) {
        return Err(errors::identity());
    }
    let name = util::safe_text(util::text(&secret, &["name"]), &credentials.protected, 100);
    let name = if name.is_empty() {
        "豆包工作".into()
    } else {
        name
    };
    Ok(
        json!({ "name": name, "nickname": name, "userId": user(&secret)?,
        "email": secret["email"].as_str().map(|value| util::safe_text(value, &credentials.protected, 254)),
        "avatar": util::safe_url(util::text(&secret, &["avatar"]), &credentials.protected),
        "tokenTail": credentials.tail(), "hasCredentials": true, "desktop": false,
        "source": if secret["source"] == "desktop" { "desktop_snapshot" } else if secret["source"] == "file" { "file" } else { "manual" },
        "available": true, "canRefresh": false, "validation": "identity_checked_on_use",
        "chatContextComplete": !credentials.field("deviceId").is_empty(),
        "migrationStage": "complete", "availabilityMessage": if credentials.field("deviceId").is_empty() {
            errors::device_context().message
        } else {
            gates::SUMMARY.to_string()
        } }),
    )
}

pub fn normalize(params: &Value) -> Result<Value> {
    credentials::own_provider(params)?;
    let (input, source, form_name) = login_methods::input(params)?;
    let expected = if params["account"].as_str().is_some_and(|id| !id.is_empty()) {
        Some(current()?)
    } else {
        None
    };
    let credentials = Credentials::parse(&input, util::now()?)?;
    credentials.chat_context()?;
    let profile = identify(&credentials)?;
    let verified_identity = format!("doubaowork-{}-{}", profile.user, profile.enterprise);
    for key in ["identity", "pluginIdentity"] {
        if input.get(key).is_some_and(|value| {
            !value.is_null() && value.as_str() != Some(verified_identity.as_str())
        }) {
            return Err(errors::identity());
        }
    }
    // 导出记录中已有身份时，它是必须核对的期望身份，不让新 Cookie 把旧人变成另一人。
    if input.get("userId").is_some_and(|value| !value.is_null()) && user(&input)? != profile.user {
        return Err(errors::identity());
    }
    if input
        .get("enterpriseType")
        .is_some_and(|value| !value.is_null())
        && enterprise(&input)? != profile.enterprise
    {
        return Err(errors::identity());
    }
    if let Some(expected) = &expected {
        if expected.user != profile.user || expected.enterprise != profile.enterprise {
            return Err(errors::identity());
        }
    }
    let supplied = params["name"]
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or(form_name)
        .or_else(|| input["name"].as_str().map(str::to_string))
        .unwrap_or_else(|| profile.name.clone());
    let name = util::safe_text(&supplied, &credentials.protected, 100);
    let mut secret = credentials.store();
    secret["userId"] = json!(profile.user);
    secret["enterpriseType"] = json!(profile.enterprise);
    secret["name"] = json!(if name.is_empty() {
        "豆包工作"
    } else {
        &name
    });
    secret["email"] = profile.email;
    secret["avatar"] = profile.avatar;
    secret["source"] = json!(source);
    secret["schema"] = json!(1);
    secret["validation"] = json!("online_identity");
    secret["validatedAt"] = json!(util::now()?);
    let identity = identity(&secret)?;
    if let Some(expected) = expected {
        expected.unchanged()?;
    }
    Ok(
        json!({ "identity": identity, "public": view(&secret)?, "secret": secret, "desktop": false }),
    )
}

pub struct Snapshot {
    record: Value,
    pub id: String,
    pub frozen_identity: String,
    pub user: String,
    pub enterprise: String,
    pub credentials: Credentials,
}

impl Snapshot {
    pub fn binding(&self) -> Value {
        let digest = util::digest(
            json!({ "credentials": self.credentials.fingerprint(),
            "accountUserAgent": self.record["public"]["userAgent"] })
            .to_string()
            .as_bytes(),
        );
        json!({ "provider": endpoints::PROVIDER, "account": self.id,
            "frozenIdentity": self.frozen_identity, "credentialDigest": digest })
    }

    pub fn unchanged(&self) -> Result<()> {
        let next = sdk::account().map_err(errors::host)?;
        if next["provider"] != endpoints::PROVIDER
            || next["id"] != self.record["id"]
            || next["identity"] != self.record["identity"]
            || next["secret"] != self.record["secret"]
            || sdk::account_disabled(&next)
            || next["public"]["userAgent"] != self.record["public"]["userAgent"]
        {
            return Err(errors::changed());
        }
        Ok(())
    }

    pub fn verify(&self) -> Result<()> {
        self.unchanged()?;
        let found = identify(&self.credentials)?;
        self.unchanged()?;
        if found.user != self.user || found.enterprise != self.enterprise {
            return Err(errors::identity());
        }
        Ok(())
    }

    pub fn read(&self, endpoint: ReadEndpoint, body: &Value) -> Result<Value> {
        self.unchanged()?;
        let result = network::read(&self.credentials, endpoint, Some(body));
        self.unchanged()?;
        result
    }
}

pub fn current() -> Result<Snapshot> {
    let record = sdk::account().map_err(errors::host)?;
    if record["provider"] != endpoints::PROVIDER || sdk::account_disabled(&record) {
        return Err(errors::changed());
    }
    let id = util::identifier(&record["id"]).ok_or_else(errors::identity)?;
    let secret = credentials::payload(&record["secret"])?;
    if record["desktop"] == true && !credentials::snapshot_present(&secret) {
        return Err(gates::legacy());
    }
    let credentials = Credentials::parse(&secret, util::now()?)?;
    let frozen_identity = identity(&secret)?;
    // 旧原生账号的宿主 identity 回落到原主键，正好也是本家的旧业务身份。
    let trusted = record["identity"]
        .as_str()
        .filter(|identity| !identity.is_empty())
        .ok_or_else(errors::identity)?;
    if trusted != frozen_identity || util::protected(&frozen_identity, &credentials.protected) {
        return Err(errors::identity());
    }
    let user = user(&secret)?;
    let enterprise = enterprise(&secret)?;
    Ok(Snapshot {
        record,
        id,
        frozen_identity,
        user,
        enterprise,
        credentials,
    })
}

pub fn ensure() -> Result<String> {
    let account = current()?;
    account.verify()?;
    Ok(account.credentials.cookie_header)
}
