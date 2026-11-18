use agent2api_wasm_sdk::{self as sdk, desktop_launch::{self as launch, Change}, json, Error, Result, Value};

use super::{account, credentials, oauth, refresh, util};

mod protobuf;

pub fn summary() -> Value {
    json!({ "version": 1, "clients": [
        { "id": "classic", "label": "Antigravity", "platform": "windows" },
        { "id": "ide", "label": "Antigravity IDE", "platform": "windows" }
    ] })
}

pub fn check() -> Result<Value> {
    let context = launch::context()?;
    if !matches!(context["client"].as_str(), Some("classic" | "ide")) { return Err(Error::unsupported()); }
    let record = sdk::account()?;
    let secret = credentials::fields(&record["secret"]);
    let available = secret["desktopReadOnly"] != true && record["public"]["desktopReadOnly"] != true
        && record["public"]["needsRelogin"] != true
        && !util::text(secret, &["refreshToken", "refresh_token"]).is_empty();
    Ok(launch::ready(available, if available { "" } else { "该账号没有可切换的完整凭证，请重新登录或导入完整凭证" }))
}

pub fn prepare() -> Result<Value> {
    let context = launch::context()?;
    let client = context["client"].as_str().ok_or_else(Error::unsupported)?;
    if !matches!(client, "classic" | "ide") || context["preparing"] != true { return Err(Error::unsupported()); }
    let before = account::current()?;
    let before_secret = credentials::parse(&before["secret"])?;
    let force = credentials::expiry(&before_secret).is_none();
    let mut record = refresh::ensure(force, Some(&before["secret"]))?;
    let mut secret = credentials::parse(&record["secret"])?;
    if secret["desktopReadOnly"] == true || record["public"]["desktopReadOnly"] == true {
        return Err(Error::http(409, "只读本机引用不能切换到另一账号"));
    }
    let mut email = util::text(&secret, &["email"]);
    if !valid_email(&email) {
        let token = credentials::token(&secret)?;
        email = oauth::email(&token, || account::unchanged(&record["secret"]))?.unwrap_or_default();
        if !valid_email(&email) { return Err(Error::http(409, "该账号缺少邮箱，无法准备本机客户端身份，请重新网页登录")); }
        let _lock = sdk::AccountLock::acquire()?;
        account::unchanged(&record["secret"])?;
        record = account::update(record, &json!({ "email": email, "userId": email }))?;
        secret = credentials::parse(&record["secret"])?;
    }
    let expires = credentials::expiry(&secret).filter(|at| *at > util::now().unwrap_or(i64::MAX))
        .ok_or_else(|| Error::http(409, "账号凭证有效期不可确认，请刷新凭证后重试"))?;
    let token = credentials::token(&secret)?;
    let refresh = util::text(&secret, &["refreshToken"]);
    let id_token = util::header(&util::text(&secret, &["idToken", "id_token"]), 16_384)?;
    let changes = if client == "classic" {
        let expiry = chrono::DateTime::from_timestamp_millis(expires).ok_or_else(|| Error::new("账号凭证有效期无效"))?
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        let mut payload = json!({ "token": {
            "access_token": token, "token_type": "Bearer", "refresh_token": refresh, "expiry": expiry,
        }, "auth_method": "consumer" });
        if !id_token.is_empty() { payload["token"]["id_token"] = json!(id_token); payload["id_token"] = json!(id_token); }
        vec![Change { resource: "oauth".into(), value: payload }]
    } else {
        let previous = launch::existing("state")?;
        let value = protobuf::state(&previous, &token, &refresh, expires / 1000, &email,
            &util::text(&secret, &["projectId", "project_id"]), &id_token)?;
        vec![Change { resource: "state".into(), value }]
    };
    account::unchanged(&record["secret"])?;
    launch::plan(client, record["secret"].clone(), changes)
}

fn valid_email(email: &str) -> bool {
    email.len() <= 480 && !email.chars().any(char::is_whitespace)
        && email.split_once('@').is_some_and(|(name, domain)| !name.is_empty() && !domain.is_empty() && !domain.contains('@'))
}
