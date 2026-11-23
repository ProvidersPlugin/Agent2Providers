//! 主程序内置的受控本机只读契约，不运行第三方辅助或提供任意读盘兜底。
//! read {} 读取官方客户端固定的 auth.json/Local State；只有 auth.json 缺失时
//! 才回退用户目录 .openclaw-autoclaw/openclaw.json 的本家 X-Authorization 投影。
//! 回包 {content:JSON文本, source:"auth-file"|"gateway-config", sourceRevision:不透明版本}。
//! decrypt {token,refreshToken} 只解 enc:，只用固定 Local State，禁止返回 DPAPI/AES 密钥。
//! 写回与只读是独立授权；续期必须经固定宿主入口核对来源后安全写回。
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
                "AutoClaw 本机登录态不可读，请先在官方客户端登录，或改用短信和明文凭据导入".into();
        }
        error
    })
}

pub fn decrypt(token: &str, refresh: &str) -> Result<(String, String)> {
    util::token_valid(token, false)?;
    util::token_valid(refresh, true)?;
    let result = helper(
        "decrypt",
        json!({ "token": token, "refreshToken": refresh }),
    )?;
    if !util::only(&result, &["token", "refreshToken"]) {
        return Err(Error::http(502, "AutoClaw 解密辅助返回了未授权字段"));
    }
    let token = util::strip_bearer(&util::text(&result, &["token"]));
    let refresh = util::strip_bearer(&util::text(&result, &["refreshToken"]));
    util::token_valid(&token, false)?;
    util::token_valid(&refresh, true)?;
    if token.starts_with("enc:") || refresh.starts_with("enc:") {
        return Err(Error::http(502, "AutoClaw 辅助未返回解密后的凭据"));
    }
    Ok((token, refresh))
}

fn raw() -> Result<Read> {
    let result = helper("read", json!({}))?;
    if !util::only(&result, &["content", "source", "sourceRevision"])
        || !matches!(
            result["source"].as_str(),
            Some("auth-file" | "gateway-config")
        )
    {
        return Err(Error::http(
            502,
            "AutoClaw 本机辅助返回了未约定的来源或字段",
        ));
    }
    let content = result["content"]
        .as_str()
        .filter(|value| !value.is_empty() && value.len() <= 256 * 1024)
        .ok_or_else(|| Error::http(502, "AutoClaw 本机辅助未返回有效凭据文本"))?;
    let payload: Value = serde_json::from_str(content)
        .map_err(|_| Error::http(502, "AutoClaw 本机凭据投影不是 JSON"))?;
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
            "AutoClaw 本机辅助只能返回本家已解密凭据投影",
        ));
    }
    if result["source"] == "gateway-config" && !account::refresh_token(&payload).is_empty() {
        return Err(Error::http(502, "AutoClaw 网关配置来源不应包含刷新令牌"));
    }
    let revision = result["sourceRevision"]
        .as_str()
        .filter(|value| !value.is_empty() && util::printable(value, 256))
        .ok_or_else(|| Error::http(502, "AutoClaw 本机辅助缺少可靠的来源修订标识"))?;
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
                "AutoClaw 本机引用不属于当前国内版读取通道",
                "forbidden",
            ));
        }
        let expected = account::user(reference);
        if expected.is_empty() {
            return Err(Error::http(
                409,
                "旧 AutoClaw 本机引用缺少用户标识，请重新导入以确认当前账号",
            ));
        }
        if account::user(&found.secret) != expected {
            return Err(Error::http(
                409,
                "AutoClaw 客户端已切换账号，请重新导入，未将另一账号的凭据用于当前记录",
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
            "AutoClaw 此来源不能写入本机续期缓存",
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
    let active = if before.compatible
        && before.value["stamp"] == current.stamp
        && before.value["secret"].is_object()
    {
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
        return Err(Error::http(409, "AutoClaw 本机账号在同步期间已变化，请重新发起"));
    }
    Ok(account::access(&current.secret))
}

pub fn reference(secret: &Value) -> Result<Value> {
    if account::user(secret).is_empty() {
        return Err(Error::http(
            400,
            "AutoClaw 本机登录态缺少可确认的用户标识，请在官方客户端重新登录",
        ));
    }
    let public = account::view(secret)?;
    Ok(
        json!({ "desktop": true, "desktopReadOnly": true, "desktopHelper": endpoints::DESKTOP_HELPER,
        "userId": account::user(secret), "deviceId": account::device(secret),
        "email": secret["email"], "tokenTail": public["tokenTail"], "hasSession": true,
        "hasRefreshToken": !account::refresh_token(secret).is_empty(), "expiresAt": account::expires(secret) }),
    )
}
