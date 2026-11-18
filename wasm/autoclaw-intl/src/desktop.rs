//! 国际版本机凭据的固定只读投影，不运行第三方辅助或提供任意读盘兜底。
//! 只读官方 auth.json/Local State；仅 auth.json 缺失时回退本家网关配置。
//! 两个客户端构建使用同一固定目录，地区由用户明确选择，不按目录猜测。
//! read {} => {provider:"autoclaw-intl",content:JSON文本,source:"auth-file"|"gateway-config",sourceRevision}；
//! content 只含 token/refreshToken/deviceId/userId/email/expiresAt，不返回路径或系统密钥。
//! decrypt 只处理本次用户明确提供的精确凭据对，不构成通用系统解密权限。
use super::{account, endpoints, util};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

const CACHE: &str = "desktop-refresh";
pub struct Read {
    pub secret: Value,
    pub stamp: String,
}

fn helper(operation: &str, payload: Value) -> Result<Value> {
    sdk::native_call(endpoints::DESKTOP_HELPER, operation, payload).map_err(|mut error| {
        if error.code.as_deref() != Some("forbidden") {
            error.status = Some(503);
            error.message =
                "AutoClaw 国际版本机登录态不可读，请先在官方国际版客户端登录，或使用明文凭据导入"
                    .into();
        }
        error
    })
}
fn owner(value: &Value) -> Result<()> {
    if value["provider"] != endpoints::PROVIDER {
        return Err(Error::coded(
            "本机辅助未确认国际版来源，已拒绝将其他地区凭据交给当前插件",
            "forbidden",
        ));
    }
    Ok(())
}
pub fn decrypt(token: &str, refresh: &str) -> Result<(String, String)> {
    util::token_valid(token, false)?;
    util::token_valid(refresh, true)?;
    let result = helper(
        "decrypt",
        json!({ "token": token, "refreshToken": refresh }),
    )?;
    owner(&result)?;
    if !util::only(&result, &["provider", "token", "refreshToken"]) {
        return Err(Error::http(502, "AutoClaw 国际版解密辅助返回了未授权字段"));
    }
    let token = util::strip_bearer(&util::text(&result, &["token"]));
    let refresh = util::strip_bearer(&util::text(&result, &["refreshToken"]));
    util::token_valid(&token, false)?;
    util::token_valid(&refresh, true)?;
    if token.starts_with("enc:") || refresh.starts_with("enc:") {
        return Err(Error::http(502, "AutoClaw 国际版辅助未返回已解密凭据"));
    }
    Ok((token, refresh))
}
fn raw() -> Result<Read> {
    let result = helper("read", json!({}))?;
    owner(&result)?;
    if !util::only(
        &result,
        &["provider", "content", "source", "sourceRevision"],
    ) || !matches!(
        result["source"].as_str(),
        Some("auth-file" | "gateway-config")
    ) {
        return Err(Error::http(
            502,
            "AutoClaw 国际版本机辅助返回了未约定的来源或字段",
        ));
    }
    let content = result["content"]
        .as_str()
        .filter(|value| !value.is_empty() && value.len() <= 256 * 1024)
        .ok_or_else(|| Error::http(502, "AutoClaw 国际版本机辅助未返回有效凭据文本"))?;
    let payload: Value = serde_json::from_str(content)
        .map_err(|_| Error::http(502, "AutoClaw 国际版本机凭据投影不是 JSON"))?;
    if !util::only(
        &payload,
        &[
            "token",
            "refreshToken",
            "deviceId",
            "userId",
            "email",
            "expiresAt",
        ],
    ) || account::access(&payload).starts_with("enc:")
        || account::refresh_token(&payload).starts_with("enc:")
    {
        return Err(Error::http(
            502,
            "AutoClaw 国际版本机辅助只能返回本家已解密凭据投影",
        ));
    }
    if result["source"] == "gateway-config" && !account::refresh_token(&payload).is_empty() {
        return Err(Error::http(502, "AutoClaw 网关配置来源不应包含刷新令牌"));
    }
    let revision = result["sourceRevision"]
        .as_str()
        .filter(|value| !value.is_empty() && util::printable(value, 256))
        .ok_or_else(|| Error::http(502, "AutoClaw 国际版本机辅助缺少可靠来源修订标识"))?;
    let secret = account::parse(&payload)?;
    let stamp =
        util::hash(&json!([endpoints::PROVIDER, result["source"], revision, content]).to_string());
    Ok(Read { secret, stamp })
}
pub fn read(reference: Option<&Value>) -> Result<Read> {
    let mut found = raw()?;
    if let Some(reference) = reference {
        if reference["desktopHelper"]
            .as_str()
            .is_some_and(|helper| helper != endpoints::DESKTOP_HELPER)
        {
            return Err(Error::coded(
                "AutoClaw 本机引用不属于当前国际版读取通道",
                "forbidden",
            ));
        }
        let expected = account::user(reference);
        if expected.is_empty() {
            return Err(Error::http(
                409,
                "旧 AutoClaw 国际版本机引用缺少用户标识，请重新导入",
            ));
        }
        if account::user(&found.secret) != expected {
            return Err(Error::http(
                409,
                "AutoClaw 客户端已切换账号，请重新导入，未使用另一账号凭据",
            )
            .with_code("identity_changed"));
        }
        if reference["desktopReadOnly"] == true {
            return Ok(found);
        }
        let cached = util::transient(CACHE)?;
        if cached.compatible
            && cached.value["stamp"] == found.stamp
            && cached.value["secret"].is_object()
        {
            let secret = account::parse(&cached.value["secret"])?;
            if account::user(&secret) == account::user(&found.secret) {
                found.secret = secret;
            }
        }
    }
    Ok(found)
}
pub fn save(previous: &account::Snapshot, next: &Value) -> Result<bool> {
    if !previous.local || previous.record["secret"]["desktopReadOnly"] == true {
        return Err(Error::coded(
            "AutoClaw 此来源不能写入国际版本机续期缓存",
            "forbidden",
        ));
    }
    let current = raw()?;
    if current.stamp != previous.stamp
        || account::user(&current.secret) != account::user(&previous.secret)
    {
        return Ok(false);
    }
    let before = util::transient(CACHE)?;
    if !before.compatible {
        return Err(Error::http(
            409,
            "AutoClaw 国际版本机续期缓存格式已变化，请重新发起",
        ));
    }
    let active = if before.value["stamp"] == current.stamp && before.value["secret"].is_object() {
        account::parse(&before.value["secret"])?
    } else {
        current.secret
    };
    if account::access(&active) != account::access(&previous.secret)
        || account::refresh_token(&active) != account::refresh_token(&previous.secret)
    {
        return Ok(false);
    }
    util::put(
        CACHE,
        &before,
        json!({ "stamp": current.stamp, "secret": account::project(next) }),
        86_400_000,
    )
}
pub fn synchronize(record: &Value) -> Result<String> {
    let current = read(Some(&record["secret"]))?;
    let mut next = reference(&current.secret)?;
    for key in ["name", "desktopReadOnly"] {
        if let Some(value) = record["secret"].get(key) { next[key] = value.clone(); }
    }
    let mut public = account::view(&next)?;
    if record["public"]["name"].is_string() { public["name"] = record["public"]["name"].clone(); }
    if account::expires(&current.secret).is_none_or(|at| at > sdk::now()) {
        public["needsRelogin"] = json!(false);
        public["needsReloginReason"] = json!("");
    }
    if sdk::host("account.update", json!({ "expect": record["secret"], "secret": next, "public": public }))?["updated"] != true {
        return Err(Error::http(409, "AutoClaw 国际本机账号在同步期间已变化，请重新发起"));
    }
    Ok(account::access(&current.secret))
}

pub fn reference(secret: &Value) -> Result<Value> {
    if account::user(secret).is_empty() {
        return Err(Error::http(
            400,
            "AutoClaw 国际版本机登录态缺少用户标识，请在官方客户端重新登录",
        ));
    }
    let public = account::view(secret)?;
    Ok(
        json!({ "provider": endpoints::PROVIDER, "region": "intl", "desktop": true, "desktopReadOnly": true,
        "desktopHelper": endpoints::DESKTOP_HELPER, "userId": account::user(secret), "deviceId": account::device(secret),
        "email": secret["email"], "tokenTail": public["tokenTail"], "hasSession": true,
        "hasRefreshToken": !account::refresh_token(secret).is_empty(), "expiresAt": account::expires(secret) }),
    )
}
