use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

use crate::{credentials, endpoints, errors, network, util};

pub fn identity(secret: &Value) -> Result<String> {
    let user = util::identifier(&secret["username"]).ok_or_else(errors::identity)?;
    Ok(format!("codar-{}", util::digest(&user)))
}

pub fn access(secret: &Value) -> Result<String> {
    if secret["source"] == "desktop" {
        return crate::desktop::access(secret);
    }
    credentials::token(secret["accessToken"].as_str().unwrap_or(""))
        .map_err(|_| Error::http(401, "科搭账号缺少有效的访问令牌，请重新登录"))
}

pub fn view(secret: &Value) -> Result<Value> {
    identity(secret)?;
    let desktop = secret["source"] == "desktop";
    let token = if desktop {
        String::new()
    } else {
        access(secret)?
    };
    let protected = vec![token.clone()];
    let name = util::safe_text(
        &util::text(secret, &["name", "nickname", "username"]),
        &protected,
        100,
    );
    let tail: String = token
        .chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let expiry = if desktop {
        secret["expiresAt"].as_i64()
    } else {
        credentials::expires(&token)
    };
    let mut view = json!({ "name": if name.is_empty() { "科搭账号" } else { &name },
        "uid": util::identifier(&secret["uid"]).or_else(|| util::identifier(&secret["username"])),
        "username": util::safe_text(&util::text(secret, &["username"]), &protected, 256),
        "nickname": util::safe_text(&util::text(secret, &["nickname"]), &protected, 100),
        "hasAccessToken": true, "tokenTail": tail, "canRefresh": !desktop,
        "desktop": desktop, "source": util::safe_text(&util::text(secret, &["source"]), &protected, 32),
        "available": expiry.is_none_or(|expires| expires > sdk::now()) });
    if let Some(expires) = expiry {
        view["expiresAt"] = json!(expires);
    }
    Ok(view)
}

pub fn from_token(raw: &str, name: Option<&str>, source: &str) -> Result<Value> {
    let token = credentials::token(raw)?;
    let now = util::now()?;
    if credentials::expires(&token).is_some_and(|expires| expires <= now) {
        return Err(Error::http(401, "科搭登录凭据已过期，请重新登录"));
    }
    let claims = credentials::claims(&token)?;
    let response = network::checked(
        "GET",
        endpoints::API,
        endpoints::PROFILE,
        &token,
        None,
        12_000,
    )?;
    let profile = network::data(&response);
    let username = util::identifier(&profile["username"])
        .ok_or_else(|| Error::http(502, "科搭没有返回有效账号名，未保存凭据"))?;
    let claim_user = util::text(&claims, &["username"]);
    if !claim_user.is_empty() && claim_user != username {
        return Err(errors::identity());
    }
    let protected = vec![token.clone()];
    let nickname = util::safe_text(
        &util::text(profile, &["display_name", "nickname"]),
        &protected,
        100,
    );
    let supplied = name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(if nickname.is_empty() {
            &username
        } else {
            &nickname
        });
    let secret = json!({ "schema": 1, "source": source, "accessToken": token, "username": username,
        "uid": util::identifier(&profile["id"]).unwrap_or(username.clone()), "nickname": nickname,
        "name": util::safe_text(supplied, &protected, 100), "validatedAt": util::now()? });
    normalized(secret)
}

pub fn normalized(secret: Value) -> Result<Value> {
    Ok(
        json!({ "identity": identity(&secret)?, "public": view(&secret)?, "desktop": secret["source"] == "desktop", "secret": secret }),
    )
}

pub fn normalize(params: &Value) -> Result<Value> {
    if params["source"]["kind"] == "desktop" {
        return crate::desktop::normalized(params["name"].as_str());
    }
    let input = credentials::input(params)?;
    let name = params["name"].as_str().or_else(|| input["name"].as_str());
    let result = from_token(input["accessToken"].as_str().unwrap_or(""), name, "manual")?;
    for key in ["uid", "username"] {
        if input
            .get(key)
            .is_some_and(|value| !value.is_null() && value != "")
            && util::identifier(&input[key]) != util::identifier(&result["secret"][key])
        {
            return Err(errors::identity());
        }
    }
    Ok(result)
}

pub struct Snapshot {
    pub record: Value,
    pub secret: Value,
    pub identity: String,
}

impl Snapshot {
    pub fn access(&self) -> Result<String> {
        access(&self.secret)
    }
    pub fn unchanged(&self) -> Result<()> {
        let current = sdk::account()?;
        if current["provider"] != endpoints::PROVIDER
            || current["id"] != self.record["id"]
            || current["identity"] != self.record["identity"]
            || current["secret"] != self.secret
            || sdk::account_disabled(&current)
            || current["public"]["userAgent"] != self.record["public"]["userAgent"]
        {
            return Err(errors::changed());
        }
        Ok(())
    }
    pub fn checked_access(&self) -> Result<String> {
        self.unchanged()?;
        let token = self.access()?;
        if self.secret["source"] == "desktop"
            && self.secret["desktopFingerprint"] != util::digest(&token)
        {
            self.verify_token(&token)?;
            if self.access()? != token {
                return Err(errors::changed());
            }
        }
        Ok(token)
    }
    pub fn verify_token(&self, token: &str) -> Result<()> {
        self.unchanged()?;
        let profile =
            network::checked("GET", endpoints::API, endpoints::PROFILE, token, None, 8000);
        self.unchanged()?;
        if self.secret["source"] == "desktop" && self.access()? != token {
            return Err(errors::changed());
        }
        let profile = profile?;
        let profile = network::data(&profile);
        let user = util::identifier(&profile["username"]).ok_or_else(errors::identity)?;
        if self.secret["username"] != user {
            return Err(errors::identity());
        }
        if let (Some(known), Some(uid)) = (
            util::identifier(&self.secret["uid"]),
            util::identifier(&profile["id"]),
        ) {
            if known != self.secret["username"].as_str().unwrap_or("") && known != uid {
                return Err(errors::identity());
            }
        }
        Ok(())
    }
    pub fn request(
        &self,
        method: &str,
        base: &str,
        path: &str,
        body: Option<&Value>,
        timeout: u64,
    ) -> Result<Value> {
        let token = self.checked_access()?;
        let result = network::checked(method, base, path, &token, body, timeout);
        self.unchanged()?;
        if self.secret["source"] == "desktop" && self.access()? != token {
            return Err(errors::changed());
        }
        result
    }
    pub fn verify(&self) -> Result<()> {
        self.verify_token(&self.access()?)
    }
}

pub fn current() -> Result<Snapshot> {
    let record = sdk::account()?;
    if record["provider"] != endpoints::PROVIDER || sdk::account_disabled(&record) {
        return Err(errors::changed());
    }
    let secret = record["secret"].clone();
    let identity = identity(&secret)?;
    if record["identity"].as_str() != Some(&identity) {
        return Err(errors::identity());
    }
    access(&secret)?;
    Ok(Snapshot {
        record,
        secret,
        identity,
    })
}
