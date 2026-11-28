//! 原生 Region::Intl 的 Zai 三步协议；所有 POST 只到国际 userapi。
//! Aliyun 票据只能来自通用宿主的真实交互，不能由表单、账号验证码池或 native helper 代填。
//! 本家原生交换协议没有客户端 code_verifier 字段；不虚构 PKCE，也不改上游授权页 challenge。
use super::{account, client, endpoints, oauth_urls, profile, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};

pub const CAPTCHA_CONFIG: &str = "/userapi/overseasv1/oauth-captcha-config";
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Zai,
}
impl Vendor {
    pub fn id(self) -> &'static str {
        match self {
            Self::Zai => "zai",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Zai => "Zai",
        }
    }
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "zai" => Ok(Self::Zai),
            _ => Err(Error::http(400, "AutoClaw 国际版只支持 Zai 授权")),
        }
    }
    fn login_path(self) -> &'static str {
        match self {
            Self::Zai => "/userapi/overseasv1/zai-oauth-login",
        }
    }
}

fn business(payload: &Value, action: &str) -> Result<()> {
    let code = util::integer(&payload["code"]);
    if code == Some(0) {
        return Ok(());
    }
    let code_text = code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "未返回".into());
    let message = match code {
        Some(631_002) => format!("本轮缺少有效的官方风控验证，请重新发起授权（业务码 {code_text}）"),
        Some(630_014) => format!("官方风控验证未通过，请重新发起授权（业务码 {code_text}）"),
        Some(631_001) => format!("官方授权码无效或已过期，请重新登录（业务码 {code_text}）"),
        Some(400_001) => format!("国际版登录请求参数被拒，请重新发起（业务码 {code_text}）"),
        Some(400_002) => format!("国际版登录签名校验失败，请校准系统时间后重试（业务码 {code_text}）"),
        Some(400_005) => format!("国际版登录请求过于频繁，请稍后再试（业务码 {code_text}）"),
        _ => format!("{action}未完成（业务码 {code_text}）"),
    };
    let mut error = Error::http(
        if code == Some(400_005) { 429 } else { 502 },
        super::errors::append_detail(format!("AutoClaw {message}"), payload),
    );
    error.upstream_code = code;
    Err(error)
}
fn config_text(data: &Value, key: &str, max: usize) -> Result<String> {
    let value = data[key].as_str().map(str::trim).unwrap_or("");
    if value.is_empty()
        || !util::printable(value, max)
        || value.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return Err(Error::http(
            502,
            "AutoClaw 国际版未返回完整有效的 Aliyun 配置",
        ));
    }
    Ok(value.into())
}
pub fn captcha_config() -> Result<Value> {
    let payload = client::post(CAPTCHA_CONFIG, "", &json!({}), "国际授权验证配置查询")?;
    business(&payload, "读取验证配置")?;
    let data = &payload["data"];
    let enabled = data["enabled"]
        .as_bool()
        .ok_or_else(|| Error::http(502, "AutoClaw 国际版验证配置缺少开关"))?;
    if !enabled {
        return Ok(
            json!({ "enabled": false, "region": "", "prefix": "", "sceneId": "", "supplier": "" }),
        );
    }
    let supplier = config_text(data, "captcha_supplier", 64)?;
    if supplier != "aliyun" {
        return Err(Error::http(
            503,
            "国际版上游返回了尚未接入的真人验证服务，未改用其他验证方式",
        ));
    }
    Ok(
        json!({ "enabled": true, "supplier": supplier, "region": config_text(data, "region", 32)?,
        "prefix": config_text(data, "prefix", 256)?, "sceneId": config_text(data, "scene_id", 256)? }),
    )
}

/// 调用者先校验官方完整回调并 CAS 标记换码中；失败后不复用授权码。
pub fn exchange(
    vendor: Vendor,
    code: &str,
    state: &str,
    navigate: &str,
    device: &str,
) -> Result<Value> {
    oauth_urls::navigate(navigate, vendor)?;
    if [code, state].iter().any(|value| {
        value.is_empty()
            || !util::printable(value, 4096)
            || value.bytes().any(|byte| byte.is_ascii_whitespace())
    }) || device.len() != 64
        || !device.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(Error::http(400, "AutoClaw 国际版授权换码参数无效"));
    }
    let payload = client::post(
        vendor.login_path(),
        "",
        &json!({ "source_id": "autoclaw", "device_id": device,
        "code": code, "state": state, "navigate_uri": navigate }),
        "国际授权换码",
    )?;
    business(&payload, "换取登录态")?;
    let data = &payload["data"];
    let token = util::strip_bearer(&util::text(data, &["access_token"]));
    let refresh = util::strip_bearer(&util::text(data, &["refresh_token"]));
    util::token_valid(&token, false)
        .map_err(|_| Error::http(502, "AutoClaw 国际版授权未返回有效访问凭据"))?;
    util::token_valid(&refresh, true)
        .map_err(|_| Error::http(502, "AutoClaw 国际版授权返回的刷新凭据无效"))?;
    if token.starts_with("enc:") || refresh.starts_with("enc:") {
        return Err(Error::http(502, "国际版 OAuth 不应返回本机密文凭据"));
    }
    // Zai OAuth 返回的 data.user_id 可能是外部账号 ID，而 AutoClaw 令牌校验与刷新一律以 JWT 中的 user_id 为准。
    let token_user = util::scalar(&util::claims(&token)["user_id"]);
    let user = if token_user.is_empty() {
        util::scalar(&data["user_id"])
    } else {
        token_user
    };
    let mut secret = account::parse(
        &json!({ "provider": endpoints::PROVIDER, "region": "intl", "token": token,
        "refreshToken": refresh, "deviceId": device, "userId": user }),
    )?;
    let username = util::text(data, &["user_name"]);
    if !username.is_empty() {
        secret["name"] = json!(username);
    }
    // 与原生相同，邮箱查询不是授权成功的前提；主机拒绝或 401 不能静默忽略，但不因外部资料 user_id 格式差异覆盖 JWT 身份。
    match profile::fetch(&secret) {
        Ok(profile) => {
            let uid = util::text(&profile, &["userId"]);
            if !uid.is_empty() && account::user(&secret).is_empty() {
                secret["userId"] = json!(uid);
            }
            let email = util::text(&profile, &["email"]);
            if !email.is_empty() {
                secret["email"] = json!(email);
            }
        }
        Err(error)
            if matches!(
                error.code.as_deref(),
                Some("forbidden" | "identity_changed")
            ) || error.status == Some(401) =>
        {
            return Err(error)
        }
        Err(_) => {}
    }
    if account::user(&secret).is_empty() {
        return Err(Error::http(
            502,
            "AutoClaw 国际版授权未给出用户身份，未添加空账号",
        ));
    }
    account::normalized(account::project(&account::parse(&secret)?))
}
