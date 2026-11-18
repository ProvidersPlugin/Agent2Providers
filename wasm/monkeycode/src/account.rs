//! 原稳定身份 monkeycode-<userId>。只接受手工 Session／完整 Cookie，
//! GET /users/status 取得真实身份后才落库；不使用调用方 userId 作为校验结果。
//! 本批不枚举历史任务发现镜像，imageId 仅保留用户明确填写的可选兼容字段。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

use crate::{
    client, credentials,
    endpoints::{self, ReadEndpoint},
    gates, util,
};

fn user_id(secret: &Value) -> Result<&str> {
    let id = util::text(secret, &["userId", "user_id"]);
    if id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) {
        return Err(Error::http(
            401,
            "MonkeyCode 账号缺少有效用户身份，请重新导入并校验登录态",
        )
        .with_code("monkeycode_identity_missing"));
    }
    Ok(id)
}

pub fn identity(secret: &Value) -> Result<String> {
    Ok(format!("monkeycode-{}", user_id(secret)?))
}

pub fn view(secret: &Value) -> Value {
    let session = credentials::session(secret).unwrap_or_default();
    let hidden = if session.is_empty() {
        util::text(secret, &["accessToken", "session", "token"])
    } else {
        &session
    };
    let id = util::public_text(util::text(secret, &["userId", "user_id"]), hidden, 128);
    let image = util::public_text(util::text(secret, &["imageId", "image_id"]), hidden, 128);
    let name = util::public_text(util::text(secret, &["name"]), hidden, 64);
    let expiry = credentials::expiry(secret);
    json!({
        "name": if name.is_empty() { format!("MonkeyCode {id}") } else { name },
        "userId": id,
        "imageId": if image.is_empty() { None } else { Some(image.clone()) },
        "hasImageId": !image.is_empty(),
        "tokenTail": credentials::tail(&session),
        "hasSession": !session.is_empty(),
        "hasAccessToken": !session.is_empty(),
        "hasRefreshToken": false,
        "canRefresh": false,
        "expiresAt": expiry,
        "expiryKnown": expiry.is_some(),
        "edition": endpoints::ID,
        "editionLabel": "国内版",
        "source": match util::text(secret, &["source"]) { "manual" => "manual", "web" => "web", _ => "existing" },
        "desktop": false,
        "chatReady": !session.is_empty() && !id.is_empty(),
        "connectionStatus": "对话、模型目录、钱包、网页登录和签到已接通；发送对话仅授权本次固定远端任务，无需额外权限设置"
    })
}

pub fn normalize(params: &Value) -> Result<Value> {
    let source = &params["source"];
    if source["kind"] != "manual" {
        return Err(Error::http(
            400,
            "MonkeyCode 本批仅支持粘贴 Session 或完整 Cookie，不读取桌面、文件或环境变量",
        )
        .with_code("monkeycode_manual_only"));
    }
    let envelope = &source["payload"];
    // 手工与完整 JSON 共用在线验证，网页登录回执由原 flow 单独进入。
    if let Some(index) = envelope.get("methodIndex").filter(|value| !value.is_null()) {
        if !matches!(index.as_u64(), Some(0 | 2)) {
            return Err(gates::web_login());
        }
    }
    let input = credentials::input(envelope)?;
    let session = input.session;
    let data = client::get(&session, ReadEndpoint::Status, &[])?;
    let user = match data.get("user") {
        Some(user) if user.is_object() => user,
        Some(_) => {
            return Err(Error::http(
                401,
                "MonkeyCode 未返回已登录用户，请重新登录后导入",
            ))
        }
        None => &data,
    };
    if user["is_blocked"] == true {
        return Err(Error::http(403, "MonkeyCode 当前用户已被停用，未保存账号"));
    }
    let id = util::text(user, &["id"]);
    if input
        .user_id
        .as_deref()
        .is_some_and(|expected| expected != id)
    {
        return Err(Error::http(
            409,
            "MonkeyCode 登录态与所导入用户身份不一致，未覆盖账号",
        ));
    }
    if id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) || id.contains(&session)
    {
        return Err(Error::http(
            401,
            "MonkeyCode 未返回有效用户身份，未添加未经校验的账号",
        ));
    }
    let explicit_name = util::text(params, &["name"]);
    let input_name = input.name.as_deref().unwrap_or("");
    let upstream_name = util::text(user, &["name", "nickname", "display_name", "email"]);
    let selected_name = if !explicit_name.is_empty() {
        explicit_name
    } else if !input_name.is_empty() {
        input_name
    } else {
        upstream_name
    };
    let name = util::public_text(selected_name, &session, 64);
    let image = input.image_id.as_deref().unwrap_or("");
    if !image.is_empty() && (!credentials::valid_image_id(image) || image.contains(&session)) {
        return Err(Error::http(400, "MonkeyCode 镜像编号无效"));
    }
    let mut secret = json!({
        "accessToken": session,
        "userId": id,
        "name": if name.is_empty() { format!("MonkeyCode {id}") } else { name },
        "source": "manual",
        "expiresAt": null,
        "expiresAtSource": "unknown"
    });
    if !image.is_empty() {
        secret["imageId"] = json!(image);
    }
    Ok(
        json!({ "identity": identity(&secret)?, "public": view(&secret),
        "secret": secret, "desktop": false }),
    )
}

pub struct Snapshot {
    pub record: Value,
    pub session: String,
}

impl Snapshot {
    pub fn binding(&self) -> String {
        util::fingerprint(&format!(
            "{}\0{}",
            util::text(&self.record["secret"], &["userId", "user_id"]),
            self.session
        ))
    }

    pub fn unchanged(&self) -> Result<()> {
        let next = sdk::account()?;
        if next["provider"] != endpoints::ID
            || next["id"] != self.record["id"]
            || next["secret"] != self.record["secret"]
            || sdk::account_disabled(&next)
        {
            return Err(util::conflict());
        }
        Ok(())
    }
}

pub fn current() -> Result<Snapshot> {
    let record = sdk::account()?;
    if record["provider"] != endpoints::ID
        || util::text(&record, &["id"]).is_empty()
        || sdk::account_disabled(&record)
    {
        return Err(Error::http(403, "MonkeyCode 当前账号未获授权或已停用"));
    }
    let id = user_id(&record["secret"])?;
    let session = credentials::session(&record["secret"])?;
    if id.contains(&session) {
        return Err(Error::http(
            401,
            "MonkeyCode 账号身份包含受保护内容，请重新导入",
        ));
    }
    Ok(Snapshot { record, session })
}

pub fn ensure() -> Result<String> {
    Ok(current()?.session)
}

pub fn expiring(params: &Value) -> Result<bool> {
    if params["account"].as_str().unwrap_or("").is_empty() {
        return Ok(false);
    }
    current()?;
    // 没有可信期限时按协议返回 false（未知），不是长期有效或完成续期。
    Ok(false)
}
