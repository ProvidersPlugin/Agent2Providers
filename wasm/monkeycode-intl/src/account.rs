//! 手工导入只以 data.user.id(UUID) 校验真实用户，不接受未经国际证实的 data.id。
//! 稳定业务身份仍为 monkeycode-<userId>；accounts.identity.v1 由宿主按 provider
//! 唯一匹配旧记录并保留主键。只保留本次明确提交的字段，不承诺合并未提交旧字段。

use crate::{
    budget::Budget,
    client, credentials,
    endpoints::{self, ReadEndpoint},
    errors, provenance, snapshot, util,
};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub fn business_id(user_id: &str) -> String {
    format!("monkeycode-{user_id}")
}

pub fn identity(secret: &Value) -> Result<String> {
    let input = credentials::parse(secret)?;
    let id = input
        .user_id
        .ok_or_else(|| Error::http(401, "国际账号缺少有效用户身份，请重新导入"))?;
    Ok(business_id(&id))
}

pub fn view(secret: &Value) -> Result<Value> {
    let input = credentials::parse(secret)?;
    let id = input
        .user_id
        .as_deref()
        .ok_or_else(|| Error::http(401, "国际账号缺少有效用户身份，请重新导入"))?;
    let name = input
        .guard
        .text(input.name.as_deref().unwrap_or("MonkeyCode 国际账号"), 64);
    let email = input
        .email
        .as_deref()
        .map(|email| input.guard.text(email, 320));
    Ok(json!({
        "name": name, "userId": id, "email": email,
        "imageId": input.image_id.as_deref().map(|image| input.guard.text(image, 128)),
        "hasImageId": input.image_id.is_some(),
        "tokenTail": credentials::tail(&input.session), "hasSession": true, "hasAccessToken": true,
        "hasRefreshToken": false, "canRefresh": false, "expiresAt": null, "expiryKnown": false,
        "edition": endpoints::ID, "editionLabel": "国际版", "desktop": false,
        "source": match secret["source"].as_str() { Some("manual") => "manual", Some("web") => "web", _ => "existing" },
        "chatReady": true, "integrationVerified": false,
        "connectionStatus": "国际站对话、目录、钱包、网页登录和签到已接通；发送对话仅授权本次固定远端任务，无需额外权限设置"
    }))
}

fn validate_expected(
    input: &credentials::Input,
    user_id: &str,
    previous: Option<&snapshot::Snapshot>,
) -> Result<()> {
    if input
        .user_id
        .as_deref()
        .is_some_and(|expected| expected != user_id)
        || previous.is_some_and(|previous| previous.user_id != user_id)
    {
        return Err(
            Error::http(409, "新登录态属于另一位用户，未覆盖旧账号或身份期望")
                .with_code("monkeycode_intl_identity_conflict"),
        );
    }
    let identity = business_id(user_id);
    if let Some(expected) = input.identity.as_deref() {
        let same_user_identity = expected == identity;
        let same_host_identity =
            previous.is_some_and(|previous| expected == previous.host_identity);
        if !same_user_identity && !same_host_identity {
            return Err(
                Error::http(409, "手动 JSON 中的身份期望与已校验账号不一致，未导入")
                    .with_code("monkeycode_intl_identity_conflict"),
            );
        }
    }
    if input
        .record_id
        .as_deref()
        .zip(previous)
        .is_some_and(|(expected, previous)| expected != previous.id)
    {
        return Err(Error::http(409, "旧记录编号与当前账号不一致，未改写主键")
            .with_code("monkeycode_intl_identity_conflict"));
    }
    Ok(())
}

pub fn normalize(params: &Value) -> Result<Value> {
    provenance::check(params)?;
    let source = &params["source"];
    if source["kind"] != "manual" {
        return Err(Error::http(
            400,
            "国际账号只接手工 Session／Cookie／JSON 或本次官方窗口回执，不读取本机文件",
        )
        .with_code("monkeycode_intl_manual_only"));
    }
    provenance::check(source)?;
    provenance::nested(&source["payload"], 0)?;
    // 只解析一次实际手工载荷；外层 NormalizeInput 已核归属，不能重复计入同一 JSON 的字节预算。
    let mut input = credentials::parse(source)?;
    if input
        .image_id
        .as_deref()
        .is_some_and(|image| !util::uuid(image))
    {
        return Err(Error::http(
            400,
            "国际账号执行环境镜像编号必须是有效非零 UUID",
        ));
    }
    let previous = snapshot::selected(params)?;
    if input.record_id.is_some() && previous.is_none() && input.user_id.is_none() {
        return Err(Error::http(
            409,
            "旧账号 JSON 缺少原用户身份，不能只按记录编号导入；请提供完整凭据或重新粘贴 Session",
        ));
    }
    // 普通添加没有旧账号上下文；只校验用户身份，输入主键不传给宿主作为覆盖目标。
    if let Some(previous) = &previous {
        validate_expected(&input, &previous.user_id, Some(previous))?;
        previous.unchanged()?;
    }
    let budget = Budget::new()?;
    let result = client::get(&input.session, ReadEndpoint::Status, &[], &budget);
    if let Some(previous) = &previous {
        previous.unchanged()?;
    }
    let document = result?;
    let data = document.data;
    input.guard.observe(&data)?;
    let user = data
        .get("user")
        .filter(|user| user.is_object())
        .ok_or_else(|| {
            errors::protocol(
                ReadEndpoint::Status,
                "缺少 data.user，未启用 data.id 兼容猜测",
            )
        })?;
    if data.get("id").is_some_and(|id| !id.is_null())
        || credentials::USER_KEYS
            .iter()
            .any(|key| data.get(*key).is_some_and(|value| !value.is_null()))
    {
        return Err(errors::protocol(
            ReadEndpoint::Status,
            "出现多个身份层级，拒绝含糊结构",
        ));
    }
    provenance::check(&data)?;
    provenance::check(user)?;
    let id = user
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| util::uuid(id))
        .ok_or_else(|| errors::protocol(ReadEndpoint::Status, "已登录用户未返回有效 UUID"))?
        .to_ascii_lowercase();
    if input.guard.contains(&id) {
        return Err(errors::protocol(ReadEndpoint::Status, "身份混入受保护内容"));
    }
    for key in credentials::USER_KEYS {
        if let Some(raw) = user.get(*key) {
            if let Some(text) = util::optional_text(raw)? {
                if !text.eq_ignore_ascii_case(&id) {
                    return Err(errors::protocol(ReadEndpoint::Status, "用户身份别名矛盾"));
                }
            }
        }
    }
    for key in ["is_blocked"] {
        if let Some(value) = user.get(key) {
            match value {
                Value::Null | Value::Bool(false) => {}
                Value::Bool(true) => return Err(errors::upstream(403, None, ReadEndpoint::Status)),
                _ => {
                    return Err(errors::protocol(
                        ReadEndpoint::Status,
                        "用户访问状态格式变化",
                    ))
                }
            }
        }
    }
    validate_expected(&input, &id, previous.as_ref())?;
    let name = params
        .get("name")
        .map(util::optional_text)
        .transpose()?
        .flatten()
        .or(input.name.as_deref())
        .or_else(|| util::text(user, "name"))
        .unwrap_or("MonkeyCode 国际账号");
    let email = input.email.as_deref().or_else(|| util::text(user, "email"));
    let mut secret = json!({
        "provider": endpoints::ID, "accessToken": input.session, "userId": id,
        "name": input.guard.text(name, 64), "source": "manual", "expiresAt": null,
        "expiresAtSource": "unknown", "identitySource": "GET data.user.id"
    });
    if let Some(email) = email {
        secret["email"] = json!(input.guard.text(email, 320));
    }
    if let Some(image) = input.image_id {
        if input.guard.contains(&image) {
            return Err(Error::http(400, "镜像编号含受保护内容，未导入"));
        }
        secret["imageId"] = json!(image);
    }
    budget.remaining()?;
    if let Some(previous) = &previous {
        previous.unchanged()?;
    }
    Ok(
        json!({ "identity": business_id(&id), "public": view(&secret)?, "secret": secret, "desktop": false }),
    )
}

pub fn ensure() -> Result<String> {
    let account = snapshot::current()?;
    account.unchanged()?;
    Ok(account.session)
}

pub fn expiring(params: &Value) -> Result<bool> {
    if let Some(account) = snapshot::selected(params)? {
        account.unchanged()?;
    }
    // 未知不是长期有效；没有可信期限，也不触发自动续期。
    Ok(false)
}
