//! 实时桌面引用只能经 desktop.credentials.raccoon.v1/read/{}。
//! 返回契约 {content,sourceId,sourceRevision}：content仅含本家auth必要字段，sourceId为
//! 固定文件的不透明标识，sourceRevision随文件变化。无路径参数、不依赖SQLite/DPAPI。
//! 旧来源 ~/.box-agent/config/auth.json；新profile须有 .raccoon-lab-client.json
//! (application=@raccoon/office-desktop,version=1)，读取 agent/config/auth.json。
//! Windows APPDATA/office-raccoon 或 ~/.office-raccoon，macOS Application Support，
//! Linux XDG_CONFIG_HOME/office-raccoon；候选选择与未标记旧profile优先规则由helper负责。

use super::{account, credentials::Credential, endpoints, iam, jwt, util};
use agent2api_wasm_sdk::{self as sdk, json, state, Error, Result, Value};

pub struct Source {
    pub credential: Credential,
    pub source_id: String,
    pub source_revision: String,
    pub fingerprint: String,
    pub payload: Value,
}

fn opaque(value: &Value, key: &str) -> Result<String> {
    value[key]
        .as_str()
        .filter(|value| !value.is_empty() && value.len() <= 256 && !value.contains('\0'))
        .map(str::to_string)
        .ok_or_else(|| Error::http(502, "小浣熊本机辅助缺少来源或版本标识"))
}

/// 仅由显式desktop归一化或账号库中已保存的desktop引用调用，不能从普通账号兜底进入。
pub fn read(reference: Option<&Value>) -> Result<Source> {
    if reference.is_some_and(|reference| {
        reference["desktop"] != true
            || reference
                .get("desktopHelper")
                .is_some_and(|helper| helper != endpoints::HELPER)
    }) {
        return Err(Error::http(403, "该账号不是已授权的小浣熊本机引用"));
    }
    let value = sdk::native_call(endpoints::HELPER, "read", json!({})).map_err(|mut error| {
        error.status = Some(503);
        error.message =
            "小浣熊本机登录态不可读，请先在官方客户端登录，或改用网页登录和手动导入".into();
        error
    })?;
    let content = value["content"]
        .as_str()
        .filter(|value| !value.is_empty() && value.len() <= endpoints::MAX_CREDENTIALS)
        .ok_or_else(|| Error::http(502, "小浣熊本机辅助未返回有效凭据文本"))?;
    let payload: Value =
        serde_json::from_str(content).map_err(|_| Error::http(502, "本机登录态不是有效JSON"))?;
    if !payload.is_object() {
        return Err(Error::http(502, "本机登录态根必须是JSON对象"));
    }
    let source_id = opaque(&value, "sourceId")?;
    let revision = opaque(&value, "sourceRevision")?;
    let mut credential = Credential::parse(&payload)?;
    let fingerprint = util::hash(
        &json!({ "source": source_id, "revision": revision, "credentials": credential.stamp() })
            .to_string(),
    );
    if let Some(reference) = reference {
        if reference
            .get("sourceId")
            .is_some_and(|id| id.as_str() != Some(source_id.as_str()))
        {
            return Err(changed());
        }
        if jwt::iam(&credential.token) {
            let known = state::conversation_get("desktop-identity", "profile", 1)?;
            if !known.compatible {
                return Err(Error::http(409, "小浣熊本机身份状态格式已变化，请重新导入"));
            }
            if known.value["source"] == fingerprint {
                credential.user = util::text(&known.value, &["userId"]);
                if util::text(&payload, &["officeIdentity", "office_identity"]).is_empty() {
                    credential.org = org(&known.value);
                }
            } else {
                let profile = iam::profile(&credential.token)?;
                credential.user = util::text(&profile, &["id"]);
                if util::text(&payload, &["officeIdentity", "office_identity"]).is_empty() {
                    credential.org = org(&profile);
                }
                if !state::conversation_compare_exchange(
                    "desktop-identity",
                    "profile",
                    1,
                    known.revision,
                    json!({ "source": fingerprint, "userId": credential.user, "officeIdentity": credential.org }),
                    2 * 3600 * 1000,
                )? {
                    return Err(Error::http(409, "小浣熊本机身份状态已更新，请重新发起"));
                }
            }
        }
        if credential.user != util::text(reference, &["userId"]) || credential.org != org(reference)
        {
            return Err(changed());
        }
        // IAM资料查询/缓存读取可能跨越官方客户端切号或换文件，再核对同一最小投影。
        let latest = sdk::native_call(endpoints::HELPER, "read", json!({}))?;
        if latest["sourceId"] != value["sourceId"]
            || latest["sourceRevision"] != value["sourceRevision"]
            || latest["content"] != value["content"]
        {
            return Err(changed());
        }
    }
    Ok(Source {
        credential,
        source_id,
        source_revision: revision,
        fingerprint,
        payload,
    })
}

fn org(secret: &Value) -> String {
    let org = util::text(secret, &["officeIdentity", "office_identity"]);
    if org == "personal" {
        String::new()
    } else {
        org
    }
}
fn changed() -> Error {
    Error::http(
        409,
        "小浣熊客户端来源、账号或组织已变化，请重新导入；不会借用新账号替换旧引用",
    )
    .with_code("desktop_identity_changed")
}

pub fn refresh_status() -> Result<Value> {
    let record = sdk::account()?;
    if record["secret"]["sourceId"].as_str().is_none_or(str::is_empty) {
        return Ok(json!({ "authorized": false, "refreshable": false }));
    }
    sdk::native_call(endpoints::REFRESH_HELPER, "status", json!({}))
}

pub fn refresh(source: &Source) -> Result<()> {
    sdk::native_call(endpoints::REFRESH_HELPER, "refresh", json!({
        "sourceId": source.source_id, "sourceRevision": source.source_revision,
    }))?;
    Ok(())
}

pub fn sync(reference: &Value, source: &Source) -> Value {
    let mut next = reference.clone();
    next["tokenTail"] = json!(util::tail(&source.credential.token));
    next["expiresAt"] = json!(source.credential.expires);
    next["hasSession"] = json!(!source.credential.token.is_empty());
    next["sourceId"] = json!(source.source_id);
    next["sourceRevision"] = json!(source.source_revision);
    next["canRefresh"] = json!(source.credential.can_refresh());
    next
}

pub fn normalize(name: Option<&str>) -> Result<Value> {
    let source = read(None)?;
    let normalized = account::normalized(&source.payload, name)?;
    let secret = &normalized["secret"];
    let user = util::text(secret, &["userId"]);
    let name = name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("桌面端登录账号（{user}）"));
    let mut reference = json!({ "desktop": true, "desktopReadOnly": true, "desktopHelper": endpoints::HELPER, "sourceId": source.source_id,
        "userId": user, "name": name, "tokenTail": normalized["public"]["tokenTail"],
        "expiresAt": normalized["public"]["expiresAt"], "hasSession": true,
        "canRefresh": normalized["public"]["canRefresh"] });
    for key in ["officeIdentity", "officeOrgName", "officeOrgRole"] {
        if let Some(value) = secret.get(key) {
            reference[key] = value.clone();
        }
    }
    let latest = read(None)?;
    if latest.source_id != source.source_id || latest.fingerprint != source.fingerprint {
        return Err(Error::http(
            409,
            "小浣熊本机登录态在导入期间已变化，请重新导入",
        ));
    }
    account::bundle(reference, true)
}
