//! 本机引用只经显式导入或已保存记录读取，普通凭据缺失不会回落本机。
//! helper只给providers.cline.settings.auth投影，不能返回完整providers.json或其他家秘密。

use super::{account, endpoints};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub struct Read {
    pub secret: Value,
    pub stamp: String,
}

fn only_fields(value: &Value, allowed: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|fields| fields.keys().all(|key| allowed.contains(&key.as_str())))
}

pub fn snapshot() -> Result<Value> {
    let reply = sdk::native_call(endpoints::DESKTOP_HELPER, "read", json!({}))?;
    let raw = reply["content"]
        .as_str()
        .filter(|raw| !raw.is_empty() && raw.len() <= 256 * 1024)
        .ok_or_else(|| Error::http(502, "本机 Cline 登录配置无效"))?;
    let payload: Value =
        serde_json::from_str(raw).map_err(|_| Error::http(502, "本机 Cline 登录配置不是 JSON"))?;
    let mut secret = account::parse(&payload)?;
    let access = account::access(&secret)?;
    let token_id = account::user_id(&access);
    if !token_id.is_empty() && token_id != account::identity(&secret) {
        return Err(
            Error::http(409, "Cline 本机凭证的账号标识不一致").with_code("identity_changed")
        );
    }
    if account::identity(&secret).starts_with("token-") {
        return Err(Error::http(
            400,
            "Cline 本机登录态缺少稳定账号身份，请重新登录官方客户端后再导入",
        ));
    }
    let public = account::view(&secret);
    if public["uid"].as_str().is_none_or(str::is_empty) {
        return Err(Error::http(400, "Cline 本机账号身份不能使用凭据本体"));
    }
    secret["displayName"] = public["displayName"].clone();
    secret["source"] = json!("desktop");
    secret["desktop"] = json!(true);
    secret["desktopReadOnly"] = json!(true);
    secret["tokenTail"] = public["tokenTail"].clone();
    secret["hasRefreshToken"] = json!(!account::text(&secret, &["refreshToken"]).is_empty());
    if let Some(fields) = secret.as_object_mut() {
        fields.remove("accessToken");
        fields.remove("refreshToken");
    }
    Ok(secret)
}

pub fn synchronize(record: &Value) -> Result<Value> {
    let current = read(&record["secret"])?;
    let mut reference = record["secret"].clone();
    let view = account::view(&current.secret);
    reference["tokenTail"] = view["tokenTail"].clone();
    reference["expiresAt"] = json!(account::expiry(&current.secret));
    reference["hasRefreshToken"] = json!(!account::text(&current.secret, &["refreshToken"]).is_empty());
    let mut public = account::view(&reference);
    if record["public"]["name"].is_string() { public["name"] = record["public"]["name"].clone(); }
    if account::expiry(&current.secret).is_none_or(|at| at > sdk::now()) {
        public["needsRelogin"] = json!(false);
        public["needsReloginReason"] = json!("");
    }
    if sdk::host("account.update", json!({ "expect": record["secret"], "secret": reference, "public": public }))?["updated"] != true {
        return Err(Error::http(409, "Cline 本机账号在同步期间已变化，请重新发起"));
    }
    Ok(current.secret)
}

pub fn read(reference: &Value) -> Result<Read> {
    if !account::local(reference) {
        return Err(Error::http(403, "当前 Cline 账号没有本机来源授权"));
    }
    let reply = sdk::native_call(endpoints::DESKTOP_HELPER, "read", json!({})).map_err(|_| {
        Error::http(
            503,
            "Cline 本机登录配置不可读，请先在官方客户端登录，或改用设备授权或上传凭据",
        )
        .with_code("native_helper_unavailable")
    })?;
    if !only_fields(&reply, &["content", "sourceRevision"]) {
        return Err(Error::http(502, "Cline 本机辅助返回了约定之外的字段"));
    }
    let content = reply["content"]
        .as_str()
        .filter(|value| value.len() <= 256 * 1024)
        .ok_or_else(|| Error::http(502, "Cline 本机辅助未返回有效文本"))?;
    let payload: Value = serde_json::from_str(content)
        .map_err(|_| Error::http(502, "Cline 本机凭证投影不是有效 JSON"))?;
    if !only_fields(&payload, &["providers"]) {
        return Err(Error::http(502, "Cline 本机辅助只能返回本家凭证投影"));
    }
    let providers = payload["providers"]
        .as_object()
        .filter(|providers| providers.len() == 1 && providers.contains_key("cline"))
        .ok_or_else(|| Error::http(502, "Cline 本机辅助只能返回本家凭证投影"))?;
    let auth = providers["cline"]
        .pointer("/settings/auth")
        .filter(|value| value.is_object())
        .ok_or_else(|| Error::http(502, "Cline 本机辅助缺少 auth 投影"))?;
    if !only_fields(&providers["cline"], &["settings"])
        || !only_fields(&providers["cline"]["settings"], &["auth"])
        || !only_fields(
            auth,
            &[
                "accessToken",
                "refreshToken",
                "expiresAt",
                "accountId",
                "metadata",
            ],
        )
        || auth
            .get("metadata")
            .filter(|metadata| !metadata.is_null())
            .is_some_and(|metadata| {
                !only_fields(metadata, &["userInfo"])
                    || metadata
                        .get("userInfo")
                        .filter(|info| !info.is_null())
                        .is_some_and(|info| {
                            !only_fields(
                                info,
                                &[
                                    "subject",
                                    "clineUserId",
                                    "email",
                                    "name",
                                    "firstName",
                                    "lastName",
                                ],
                            )
                        })
            })
    {
        return Err(Error::http(502, "Cline 本机辅助返回的投影包含未授权字段"));
    }
    let mut secret = account::parse(auth)?;
    let token_id = account::user_id(&account::access(&secret)?);
    if !token_id.is_empty() && token_id != account::identity(&secret) {
        return Err(
            Error::http(409, "Cline 本机凭证的账号标识不一致").with_code("identity_changed")
        );
    }
    let expected = account::identity(reference);
    let actual = account::identity(&secret);
    let legacy_display = account::text(reference, &["account", "userId", "accountId"]);
    let current_display = account::text(&secret, &["displayName"]);
    // 老原生导入在缺 accountId 时曾把邮箱写入 account，而新投影优先使用JWT本家UID。
    // 只兼容没有只读新标记且原存储邮箱逐字等于当前官方展示邮箱的引用，不以重名或摘要补造身份。
    let legacy_matches = reference["desktopReadOnly"] != true
        && !legacy_display.is_empty()
        && !legacy_display.starts_with("token-")
        && legacy_display == current_display
        && legacy_display.contains('@');
    if expected.is_empty() || actual != expected && !legacy_matches {
        return Err(Error::http(
            409,
            "Cline 客户端已切换账号或旧引用无法确认身份，请重新通过设备授权添加",
        )
        .with_code("identity_changed"));
    }
    if legacy_matches && actual != expected {
        secret["account"] = json!(expected);
    }
    let name = account::text(reference, &["displayName", "name"]);
    if !name.is_empty() {
        secret["displayName"] = json!(name);
    }
    let revision = reply["sourceRevision"].as_str().unwrap_or("");
    if revision.len() > 256 || revision.chars().any(char::is_control) {
        return Err(Error::http(502, "Cline 本机来源版本格式异常"));
    }
    let stamp =
        account::fingerprint(&json!({ "revision": revision, "content": content }).to_string());
    Ok(Read { secret, stamp })
}
