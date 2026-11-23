//! 短信/密码成功后共用收尾：Cookie→官方init-app→userInfo→bot/models/configs。
//! 不拿网页token冒充模型key；不保存密码、短信码或滑块结果。

use super::{account, client, endpoints};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn complete(login: &Value, source: &str, current: impl Fn() -> Result<()>) -> Result<Value> {
    current()?;
    let account_id = ["account_id", "accountId"]
        .iter()
        .map(|key| account::scalar(&login[*key]))
        .find(|value| !value.is_empty())
        .unwrap_or_default();
    let sso = account::text(login, &["ssoSessionId"]);
    let mut token = account::text(login, &["token"]);
    if token.is_empty() {
        token = sso.clone();
    }
    if account_id.is_empty() || token.is_empty() {
        return Err(Error::http(502, "AStudio 登录响应缺少账号或网页会话"));
    }
    let mut secret = json!({ "uid": account::login_uid(&login["uid"]), "source": source,
        "session": { "token": token, "accountId": account_id } });
    if !sso.is_empty() {
        secret["session"]["ssoSessionId"] = json!(sso);
    }
    // 先校验Cookie原料，不能将上游异常字段变成额外Cookie。
    account::cookie(&secret)?;
    let init =
        client::studio("POST", endpoints::INIT_APP, Some(&secret))?.data(false, "session")?;
    match init["banned"].as_bool() {
        Some(false) => {}
        Some(true) => {
            return Err(Error::http(403, "AStudio 账号已被封禁").with_code("astudio_banned"))
        }
        None => return Err(Error::http(502, "AStudio 登录初始化没有确认账号状态")),
    }
    current()?;
    let mut mobile = account::text(login, &["mobile", "phoneNum", "phone"]);
    let mut nickname = String::new();
    if secret["uid"].as_str().unwrap_or("").is_empty() || mobile.is_empty() {
        match client::studio("GET", endpoints::USER_INFO, Some(&secret))
            .and_then(|reply| reply.data(false, "session"))
        {
            Ok(data) => {
                let info = data.get("userInfo").unwrap_or(&data);
                if secret["uid"].as_str().unwrap_or("").is_empty() {
                    secret["uid"] = json!(account::login_uid(&info["uid"]));
                }
                if mobile.is_empty() {
                    mobile = account::text(info, &["mobile", "telephone"]);
                }
                nickname = account::text(info, &["nickname", "username"]);
            }
            Err(error) if error.code.as_deref() == Some("astudio_session_expired") => {
                return Err(error)
            }
            Err(_) => {}
        }
    }
    current()?;
    resolve_model(&mut secret, &current)?;
    let uid = account::scalar(&secret["uid"]);
    let name = if !mobile.is_empty() {
        mobile
    } else if !nickname.is_empty() {
        nickname
    } else if !uid.is_empty() {
        uid
    } else {
        "AStudio".into()
    };
    secret["name"] = json!(account::safe_text(&name, 100));
    account::normalized(account::parse(&secret, source)?, None)
}

/// 本次有效网页会话换模型凭据是只读查询，不调用每日登录初始化、不写客户端。
pub fn resolve_model(secret: &mut Value, current: impl Fn() -> Result<()>) -> Result<()> {
    current()?;
    account::cookie(secret)?;
    let data =
        client::studio("GET", endpoints::MODEL_CONFIGS, Some(secret))?.data(false, "session")?;
    current()?;
    let list = data
        .as_array()
        .filter(|list| list.len() <= 1024)
        .ok_or_else(|| Error::http(502, "AStudio 模型凭据配置格式异常"))?;
    let valid_key = |row: &Value| account::text(row, &["api_key"]);
    let pick = |flag: &str| {
        list.iter()
            .filter(|row| row[flag] == true)
            .map(valid_key)
            .find(|key| !key.is_empty())
    };
    let key = pick("is_current")
        .or_else(|| pick("is_default"))
        .or_else(|| list.iter().map(valid_key).find(|key| !key.is_empty()))
        .ok_or_else(|| Error::http(502, "这个 AStudio 账号没有可用模型凭据"))?;
    secret["key"] = json!(key);
    account::model_key(secret)?;
    Ok(())
}
