use agent2api_wasm_sdk::{self as sdk, json, Result, Value};

use crate::{
    credentials::{self, Cookies},
    endpoints, errors, network, util,
};

pub fn identity(secret: &Value) -> Result<String> {
    let uid = util::identifier(&secret["uid"]).ok_or_else(errors::identity)?;
    Ok(format!("dumate-{}", util::digest(&uid)))
}

fn profile(cookie: &Cookies) -> Result<Value> {
    let result = network::web(
        "GET",
        endpoints::USER,
        cookie,
        None,
        &cookie.protected(&Value::Null),
    )?;
    let keys = if cookie.origin == endpoints::CONSOLE {
        ["bceUserId", "uid", "userId", "user_id"]
    } else {
        ["uid", "userId", "user_id", "bceUserId"]
    };
    let uid = keys
        .iter()
        .find_map(|key| util::identifier(&result[*key]))
        .ok_or_else(errors::identity)?;
    let protected = cookie.protected(&Value::Null);
    if protected
        .iter()
        .any(|secret| secret.len() >= 8 && uid.contains(secret))
    {
        return Err(errors::identity());
    }
    let account = util::identifier(&result["bceAccountId"]);
    if account.as_ref().is_some_and(|account| {
        protected
            .iter()
            .any(|value| value.len() >= 8 && account.contains(value))
    }) {
        return Err(errors::identity());
    }
    Ok(json!({ "uid": uid, "bceAccountId": account,
        "nickname": util::safe_text(&util::text(&result, &["nickname", "displayName", "name"]), &protected, 100) }))
}

pub fn view(secret: &Value) -> Result<Value> {
    identity(secret)?;
    let cookie = Cookies::parse(secret)?;
    let protected = cookie.protected(secret);
    let name = util::safe_text(&util::text(secret, &["name", "nickname"]), &protected, 100);
    let mut view = json!({ "name": if name.is_empty() { "百度搭子账号" } else { &name },
        "nickname": util::safe_text(&util::text(secret, &["nickname"]), &protected, 100),
        "uid": secret["uid"], "tokenTail": cookie.tail(), "hasCookie": true,
        "hasAccessToken": !util::text(secret, &["appToken"]).is_empty(), "canRefresh": true,
        "desktop": false, "available": true,
        "source": if secret["source"] == "desktop_snapshot" { "desktop_snapshot" } else { "cookie_snapshot" } });
    if let Some(expires) = util::timestamp(&secret["appTokenExpiresAt"]) {
        view["expiresAt"] = json!(expires);
    }
    Ok(view)
}

pub fn normalize(params: &Value) -> Result<Value> {
    if params
        .get("provider")
        .is_some_and(|provider| provider != endpoints::PROVIDER)
    {
        return Err(errors::identity());
    }
    let previous = if params["account"].as_str().is_some_and(|id| !id.is_empty()) {
        Some(current()?)
    } else {
        None
    };
    let (input, form_name) = credentials::input(params)?;
    let cookie = Cookies::parse(&input)?;
    let profile = profile(&cookie)?;
    for value in [&input, &input["profile"]] {
        if value
            .get("loginType")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind == "enterprise")
            || value
                .get("enterpriseApiBaseUrl")
                .is_some_and(|base| !base.is_null() && base != "")
        {
            return Err(sdk::Error::http(
                400,
                "百度搭子企业/自定义服务器账号尚未接入此插件，请使用个人账号 Cookie",
            ));
        }
        for key in ["uid", "userId", "user_id", "bceUserId"] {
            if value
                .get(key)
                .is_some_and(|value| !value.is_null() && value != "")
                && util::identifier(&value[key]) != util::identifier(&profile["uid"])
            {
                return Err(errors::identity());
            }
        }
        if value
            .get("bceAccountId")
            .is_some_and(|value| !value.is_null() && value != "")
            && util::identifier(&value["bceAccountId"])
                != util::identifier(&profile["bceAccountId"])
        {
            return Err(errors::identity());
        }
    }
    let new_identity = identity(&profile)?;
    if input
        .get("identity")
        .is_some_and(|value| !value.is_null() && value.as_str() != Some(&new_identity))
        || previous
            .as_ref()
            .is_some_and(|account| account.identity != new_identity)
    {
        return Err(errors::identity());
    }
    let supplied = util::text(params, &["name"]);
    let name = if !supplied.is_empty() {
        supplied
    } else {
        form_name.unwrap_or_else(|| {
            let name = util::text(&input, &["name", "nickname"]);
            if name.is_empty() {
                util::text(&input["profile"], &["displayName"])
            } else {
                name
            }
        })
    };
    let name = if name.is_empty() {
        util::text(&profile, &["nickname"])
    } else {
        name
    };
    let mut secret = json!({ "schema": 1, "cookie": cookie.header,
        "uid": profile["uid"], "nickname": profile["nickname"], "bceAccountId": profile["bceAccountId"],
        "webOrigin": cookie.origin,
        "name": util::safe_text(&name, &cookie.protected(&Value::Null), 100), "validatedAt": util::now()? });
    if let Some(rows) = &cookie.rows {
        secret["cookies"] = json!(rows);
    }
    if params["source"]["kind"] == "desktop" {
        secret["source"] = json!("desktop_snapshot");
    }
    crate::token::exchange(&mut secret)?;
    if let Some(previous) = previous {
        previous.unchanged()?;
    }
    Ok(
        json!({ "identity": identity(&secret)?, "public": view(&secret)?, "secret": secret, "desktop": false }),
    )
}

pub struct Snapshot {
    pub record: Value,
    pub secret: Value,
    pub cookie: Cookies,
    pub identity: String,
}

impl Snapshot {
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

    pub fn verify(&self) -> Result<()> {
        self.unchanged()?;
        let found = profile(&self.cookie)?;
        self.unchanged()?;
        if found["uid"] != self.secret["uid"]
            || self
                .secret
                .get("bceAccountId")
                .is_some_and(|id| !id.is_null() && id != "" && id != &found["bceAccountId"])
        {
            return Err(errors::identity());
        }
        Ok(())
    }

    pub fn web(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        self.unchanged()?;
        let result = network::web(
            method,
            path,
            &self.cookie,
            body,
            &self.cookie.protected(&self.secret),
        );
        self.unchanged()?;
        result
    }

    pub fn protected(&self) -> Vec<String> {
        self.cookie.protected(&self.secret)
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
    let cookie = Cookies::parse(&secret)?;
    Ok(Snapshot {
        record,
        secret,
        cookie,
        identity,
    })
}
