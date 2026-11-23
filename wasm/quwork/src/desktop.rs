//! 本机文件只通过受限 helper 读取；调用失败必须如实报告。
//! helper 固定读取 SQLite auth_session/schema_version=2 的 session_json；只有无库
//! 才可回落 DSH 模型凭证。它返回解析后的 UTF-8 JSON，不接受调用者指定路径。
//! 缺失设备标识不触发第二条读盘旁路，兑换激活由用户补充原 deviceIdentifier。

use super::{account, client, endpoints};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub fn read(reference: Option<&Value>) -> Result<Value> {
    let response =
        sdk::native_call(endpoints::HELPER, "read", json!({})).map_err(|mut error| {
            error.status = Some(503);
            error.message =
                "QuWork 本机登录态不可读，请先在官方客户端登录，或使用手动凭据导入".into();
            error
        })?;
    let content = response["content"]
        .as_str()
        .filter(|text| text.len() <= endpoints::MAX_CREDENTIALS)
        .ok_or_else(|| Error::http(502, "QuWork 本机辅助未返回有效凭证文本"))?;
    let raw: Value = serde_json::from_str(content)
        .map_err(|_| Error::http(502, "QuWork 本机辅助返回的凭证不是 JSON"))?;
    let mut secret = account::parse(&raw)?;
    if secret["expiresAt"].is_null() {
        if let Some(updated) = client::timestamp(&raw["updatedAt"]) {
            secret["expiresAt"] = json!(updated.saturating_add(7_200_000));
        }
    }
    if let Some(reference) = reference {
        if reference
            .get("desktopHelper")
            .is_some_and(|helper| helper.as_str() != Some(endpoints::HELPER))
            || reference
                .get("sourceId")
                .is_some_and(|source| source.as_str() != response["sourceId"].as_str())
        {
            return Err(Error::http(409, "QuWork 本机关联来源已变化，请重新导入")
                .with_code("identity_changed"));
        }
        let expected = account::text(&reference["user"], &["uid", "id"]);
        if !expected.is_empty() && account::text(&secret["user"], &["uid", "id"]).is_empty() {
            let access = account::text(&secret, &["accessToken"]);
            if access.is_empty() {
                return Err(
                    Error::http(409, "QuWork 本机来源缺少原账号标识，请重新导入")
                        .with_code("identity_changed"),
                );
            }
            let user =
                client::request("GET", endpoints::SELF, &access, None)?.data("本机账号核对")?;
            if !user.is_object() {
                return Err(Error::http(502, "QuWork 本机账号信息格式异常"));
            }
            secret["user"] = account::user_view(&user);
        }
        if account::identity(&secret) != account::identity(reference) {
            return Err(Error::http(
                409,
                "QuWork 客户端已切换账号，请重新导入；不会用另一账号替换现有记录",
            )
            .with_code("identity_changed"));
        }
        let name = account::text(reference, &["name"]);
        if !name.is_empty() {
            secret["name"] = json!(name);
        }
    }
    if let Some(source) = response["sourceId"]
        .as_str()
        .filter(|source| !source.is_empty() && source.len() <= 128)
    {
        secret["desktopSourceId"] = json!(source);
    }
    Ok(secret)
}

pub fn reference_from(secret: &Value) -> Value {
    let llm = account::text(secret, &["llmToken"]);
    let view = account::view(secret);
    let mut reference = json!({ "source": "desktop", "desktopHelper": endpoints::HELPER,
        "user": secret["user"],
        "modelFingerprint": account::fingerprint(&llm), "tokenTail": view["tokenTail"],
        "hasLoginToken": !account::text(secret, &["accessToken"]).is_empty(),
        "expiresAt": secret["expiresAt"], "updatedAt": secret["updatedAt"] });
    if secret["desktopSourceId"].is_string() {
        reference["sourceId"] = secret["desktopSourceId"].clone();
    }
    reference
}

pub fn reference() -> Result<Value> {
    let mut secret = read(None)?;
    let access = account::text(&secret, &["accessToken"]);
    if !access.is_empty() && account::text(&secret["user"], &["uid", "id"]).is_empty() {
        if let Ok(user) = client::request("GET", endpoints::SELF, &access, None)
            .and_then(|reply| reply.data("本机账号核对"))
        {
            let verified = account::user_view(&user);
            if !account::text(&verified, &["uid", "id"]).is_empty() {
                secret["user"] = verified;
            }
        }
        // 仅模型凭据仍保留旧的密钥摘要身份；缺少用户资料时不伪造稳定用户标识。
    }
    Ok(reference_from(&secret))
}

pub fn stamp(secret: &Value) -> String {
    account::fingerprint(&json!({ "access": secret["accessToken"], "model": secret["llmToken"],
        "expires": secret["expiresAt"], "updated": secret["updatedAt"], "device": secret["deviceIdentifier"] }).to_string())
}
