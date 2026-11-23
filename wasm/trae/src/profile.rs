//! 显示资料投影不等于可信登录身份。网页登录只接受本轮GetUserInfo的真实UID；
//! 缺失/异常时明确未确认，不以回调userInfo或unknown占位生成登录完成账号。

use super::{client, credentials, endpoints, headers, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub const IDENTITY_UNCONFIRMED: &str = "trae_oauth_identity_unconfirmed";

pub fn project(value: &Value) -> Value {
    let take = |keys: &[&str]| {
        keys.iter()
            .map(|key| util::scalar(&value[*key]))
            .find(|value| !value.is_empty())
            .unwrap_or_default()
    };
    json!({ "uid": take(&["UserID", "userId", "uid", "UID", "user_id"]),
        "nickname": take(&["ScreenName", "screenName", "Nickname", "nickname", "Name", "name", "displayName"]),
        "enterpriseId": take(&["EnterpriseID", "enterpriseId", "TenantID", "tenantId"]) })
}

pub fn fetch(secret: &Value) -> Result<Value> {
    fetch_checked(secret, || Ok(()))
}

pub fn fetch_checked(secret: &Value, current: impl Fn() -> Result<()>) -> Result<Value> {
    let token = credentials::token(secret)?;
    let body = json!({ "ReqSource": "IDE", "IDEVersion": endpoints::IDE_VERSION });
    let mut last = None;
    for host in util::account_hosts(&util::text(secret, &["apiHost"]), false)? {
        current()?;
        match client::post(
            &format!("{host}{}", endpoints::PROFILE),
            headers::oauth(Some(&token)),
            &body,
        )
        .and_then(|reply| reply.accepted())
        {
            Ok(data) => {
                let mut source = None;
                for field in ["Result", "result"] {
                    if let Some(value) = data.get(field).filter(|value| !value.is_null()) {
                        if !value.is_object() || source.is_some_and(|previous| previous != value) {
                            return Err(unconfirmed());
                        }
                        source = Some(value);
                    }
                }
                let source = source.unwrap_or(&data);
                let mut projected = project(source);
                // 与回调投影分开：只有本次真实GetUserInfo请求的有效UID能进入登录成功。
                projected["uid"] = json!(authoritative_uid(source)?);
                return Ok(projected);
            }
            Err(error) => {
                last = Some(error);
            }
        }
    }
    Err(last.unwrap_or_else(|| agent2api_wasm_sdk::Error::http(502, "Trae 账号资料未能读取")))
}

pub fn merged(primary: &Value, _callback: &Value, _variant: &str) -> Value {
    // 旧调用签名暂留兼容；此登录路径绝不从回调补身份、昵称或租户。
    let uid = primary
        .get("uid")
        .and_then(Value::as_str)
        .filter(|uid| valid_uid(uid))
        .unwrap_or("");
    json!({ "uid": uid, "nickname": util::text(primary, &["nickname"]),
        "enterpriseId": util::text(primary, &["enterpriseId"]) })
}

fn authoritative_uid(source: &Value) -> Result<String> {
    if !source.is_object() {
        return Err(unconfirmed());
    }
    let mut found: Option<String> = None;
    for field in ["UserID", "userId", "uid", "UID", "user_id"] {
        let Some(value) = source.get(field).filter(|value| !value.is_null()) else {
            continue;
        };
        let uid = match value {
            Value::String(text) if text.is_empty() => continue,
            Value::String(text) => text.clone(),
            Value::Number(number) => number
                .as_u64()
                .filter(|number| *number > 0)
                .map(|number| number.to_string())
                .ok_or_else(unconfirmed)?,
            _ => return Err(unconfirmed()),
        };
        if !valid_uid(&uid) || found.as_ref().is_some_and(|previous| previous != &uid) {
            return Err(unconfirmed());
        }
        found = Some(uid);
    }
    found.ok_or_else(unconfirmed)
}

fn valid_uid(uid: &str) -> bool {
    !uid.is_empty()
        && uid.len() <= 480
        && uid.trim() == uid
        && !uid.chars().any(char::is_control)
        && !matches!(uid, "unknown-cn" | "solo-unknown")
}

pub fn unconfirmed() -> Error {
    Error::http(
        502,
        "Trae 本轮授权后的真实账号身份未确认，未保存账号；不会使用回调身份或unknown占位",
    )
    .with_code(IDENTITY_UNCONFIRMED)
}

/// login.rs会把GetUserInfo错误折成空值；在唯一login.callback回包出口阻止空身份进入Done。
/// 不对旧账号/account.view/manual导入施加此门，不改已保存ID或凭据。
pub fn login_result(value: Value) -> Result<Value> {
    if value["state"].as_str() == Some("done") {
        let uid = value["account"]["secret"]["uid"]
            .as_str()
            .filter(|uid| valid_uid(uid))
            .ok_or_else(unconfirmed)?;
        if value["account"]["public"]["uid"].as_str() != Some(uid) {
            return Err(unconfirmed());
        }
    }
    Ok(value)
}

pub fn stops_retry(body: &Value) -> bool {
    [
        body.get("providerCode"),
        body.get("code"),
        body.pointer("/error/providerCode"),
        body.pointer("/error/code"),
    ]
    .into_iter()
    .flatten()
    .any(|value| value.as_str() == Some(IDENTITY_UNCONFIRMED))
}
