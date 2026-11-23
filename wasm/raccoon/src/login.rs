//! 内嵌授权走官方自定义协议，外部网页登录走本轮登记的HTTP回环地址。
//! 使用宿主可信flow作state，自动捕获与完整URL回填共用严格校验；不接受无state裸code。
//! 一次性换码状态保存在Scope::Login，取消/重入CAS阻止复活，敏感code不进入公开状态。

use super::{account, endpoints, errors, jwt, network, util};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope, Snapshot},
    Error, Result, Value,
};

const KEY: &str = "authorization";
const TTL: i64 = 10 * 60 * 1000;
fn flow(params: &Value) -> Result<&str> {
    params["flow"]
        .as_str()
        .filter(|value| !value.is_empty() && value.len() <= 256 && !value.contains('\0'))
        .ok_or_else(|| Error::http(400, "主程序未提供可信小浣熊登录流程"))
}
fn load() -> Result<Snapshot> {
    let value = state::get(Scope::Login, KEY, 1)?;
    if !value.compatible {
        return Err(Error::http(409, "小浣熊登录状态格式已变化，请重新发起"));
    }
    Ok(value)
}
fn save(snapshot: &Snapshot, value: Value) -> Result<bool> {
    let ttl = value["expires"]
        .as_i64()
        .map(|at| at.saturating_sub(sdk::now()).clamp(1000, TTL))
        .unwrap_or(1000);
    state::compare_exchange(Scope::Login, KEY, 1, snapshot.revision, value, ttl)
}
fn failed(message: &str) -> Value {
    json!({ "state": "failed", "message": message })
}

pub fn start(params: &Value) -> Result<Value> {
    let flow = flow(params)?;
    if params["method"]
        .as_str()
        .is_some_and(|value| !matches!(value, "" | "browser"))
        || params["payload"]["methodIndex"].as_u64().unwrap_or(0) != 0
    {
        return Err(Error::http(400, "小浣熊请使用网页登录或手动凭据入口"));
    }
    let external = params["payload"]["mode"] == "external";
    let callback = if external {
        let raw = params["payload"]["callbackOrigin"].as_str().unwrap_or("");
        let origin = url::Url::parse(raw).map_err(|_| Error::http(400, "小浣熊网页回跳地址无效"))?;
        if origin.scheme() != "http" || !matches!(origin.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
            || origin.port().is_none_or(|port| port == 0) || origin.path() != "/" || origin.query().is_some() || origin.fragment().is_some()
            || !origin.username().is_empty() || origin.password().is_some() {
            return Err(Error::http(400, "小浣熊回跳需 HTTP 回环地址和端口，远程面板请先做本机端口转发"));
        }
        format!("{}/api/session/login/remote-callback", origin.origin().ascii_serialization())
    } else { "office-raccoon://auth/callback".to_string() };
    let mut url = url::Url::parse(&format!("{}/authorize", endpoints::ORIGIN))
        .map_err(|_| Error::new("小浣熊授权地址无效"))?;
    url.query_pairs_mut()
        .append_pair("login_source", if external { "web" } else { "desktop" })
        .append_pair("appname", "办公小浣熊客户端")
        .append_pair("state", flow);
    if external {
        let mut target = url::Url::parse(&callback).map_err(|_| Error::new("小浣熊回跳地址无效"))?;
        target.query_pairs_mut().append_pair("state", flow);
        url.query_pairs_mut().append_pair("redirect", target.as_str());
    }
    let previous = load()?;
    if previous.value.is_object() {
        return Err(Error::http(409, "小浣熊登录已发起，请使用当前授权页面"));
    }
    if !save(
        &previous,
        json!({ "phase": "waiting", "callback": callback, "expires": sdk::now().saturating_add(TTL) }),
    )? {
        return Err(Error::http(409, "小浣熊登录流程已变化"));
    }
    Ok(
        json!({ "flow": flow, "url": url.to_string(), "poll_interval_ms": 0,
        "instructions": "",
        "external_browser": external, "allowed_hosts": ["xiaohuanxiong.com", "sensetime.com", "aliyun.com", "aliyuncs.com", "alicdn.com", "qq.com", "baidu.com"],
        "login_ui": { "browserMode": "choice", "manualCallback": true, "callbackInput": "url", "cookieCapture": null, "captureCallback": true } }),
    )
}

pub fn poll(params: &Value) -> Result<Value> {
    flow(params)?;
    let value = load()?.value;
    if value["phase"] == "done" {
        return Ok(sdk::login::done(value["account"].clone()));
    }
    if value["phase"] == "failed" {
        return Ok(failed(
            value["message"].as_str().unwrap_or("小浣熊登录失败"),
        ));
    }
    if value["phase"] == "cancelled" || value["expires"].as_i64().unwrap_or(0) <= sdk::now() {
        return Ok(failed("小浣熊登录已取消或过期，请重新发起"));
    }
    Ok(json!({ "state": "pending", "hint": "等待本次官方登录回调" }))
}

fn code(raw: &str, expected: &str, callback: &str) -> Result<Option<String>> {
    if raw.len() > 8192 || raw.contains('\\') || raw.chars().any(|ch| ch.is_control() || ch.is_whitespace()) {
        return Err(Error::http(400, "请提交完整的小浣熊登录回调地址"));
    }
    let url = url::Url::parse(raw)
        .map_err(|_| Error::http(400, "请提交完整的小浣熊登录回调地址"))?;
    if raw.split_once('?').map(|(base, _)| base) != Some(callback)
        || !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(Error::http(400, "小浣熊登录回调地址与本次流程不一致"));
    }
    let mut fields = std::collections::BTreeMap::new();
    for (key, value) in url.query_pairs() {
        if key.is_empty() || fields.len() >= 32 || value.len() > 8192 || value.chars().any(char::is_control)
            || fields.insert(key.into_owned(), value.into_owned()).is_some() {
            return Err(Error::http(400, "小浣熊回调参数重复或无效"));
        }
    }
    if fields.get("state").map(String::as_str) != Some(expected) {
        return Err(Error::http(400, "小浣熊回调不属于当前登录流程"));
    }
    if fields.get("error").is_some_and(|value| !value.is_empty()) { return Ok(None); }
    let code = fields.get("code").or_else(|| fields.get("authorization_code"))
        .filter(|value| !value.trim().is_empty()).ok_or_else(|| Error::http(400, "小浣熊回调缺少授权码"))?;
    if fields.contains_key("code") && fields.contains_key("authorization_code") {
        return Err(Error::http(400, "小浣熊回调包含多个授权码"));
    }
    Ok(Some(code.to_string()))
}

pub fn callback(params: &Value) -> Result<Value> {
    let flow = flow(params)?;
    let previous = load()?;
    if previous.value["phase"] != "waiting" {
        return poll(params);
    }
    if previous.value["expires"].as_i64().unwrap_or(0) <= sdk::now() {
        return poll(params);
    }
    let raw = params["url"]
        .as_str()
        .filter(|raw| !raw.is_empty())
        .or_else(|| params["code"].as_str())
        .unwrap_or("");
    let callback = previous.value["callback"].as_str().unwrap_or("office-raccoon://auth/callback");
    let code = match code(raw, flow, callback) {
        Ok(Some(code)) => code,
        Ok(None) => {
            save(&previous, json!({"phase":"failed","message":"小浣熊官方授权已取消","expires":previous.value["expires"]}))?;
            return poll(params);
        }
        Err(error) => return Ok(json!({"state":"pending","retryable":true,"hint":error.message})),
    };
    let marker = util::random_hex(16)?;
    let mut reserved = previous.value.clone();
    reserved["phase"] = json!("exchanging");
    reserved["marker"] = json!(marker);
    if !save(&previous, reserved)? {
        return poll(params);
    }
    let reserved = load()?;
    if reserved.value["phase"] != "exchanging" || reserved.value["marker"] != marker {
        return poll(params);
    }
    let result = exchange(&code);
    let next = match result {
        Ok(account) => {
            json!({ "phase": "done", "account": account, "expires": previous.value["expires"] })
        }
        Err(error) => {
            json!({ "phase": "failed", "message": error.message, "expires": previous.value["expires"] })
        }
    };
    if !save(&reserved, next)? {
        return Ok(failed("小浣熊登录已取消或更新，未添加账号"));
    }
    poll(params)
}

fn exchange(code: &str) -> Result<Value> {
    let reply = network::raw(
        "POST",
        &format!("{}/login_with_authorization_code", endpoints::AUTH),
        "",
        "",
        json!({}),
        Some(&json!({ "authorization_code": code })),
    )?;
    if errors::code(&reply.value) == Some(200_035) {
        return Err(Error::http(400, "授权码已失效，请重新发起网页登录"));
    }
    let data = network::data(reply, &[code], true)?;
    let token = jwt::strip(&util::text(&data, &["access_token"]));
    let refresh = jwt::strip(&util::text(&data, &["refresh_token"]));
    if token.is_empty() || !jwt::iam(&token) && refresh.is_empty() {
        return Err(Error::http(502, "小浣熊登录响应缺少完整凭据"));
    }
    account::normalized(&data, None)
}

pub fn cancel(params: &Value) -> Result<Value> {
    flow(params)?;
    for _ in 0..3 {
        let previous = load()?;
        if save(&previous, json!({ "phase": "cancelled" }))? {
            return Ok(Value::Null);
        }
    }
    Err(Error::http(409, "小浣熊登录状态正在变化，未覆盖其他流程"))
}

pub fn browser_instructions(locale: &str) -> &'static str {
    match locale {
        "en" => "Sign in on the official Raccoon page to add the account; when accessing the panel remotely, paste the callback URL if it does not return automatically.",
        "zh-Hant" => "在小浣熊官方頁面完成登入即可新增帳號；遠端存取面板且未自動返回時，按提示貼上回呼網址即可。",
        "ja" => "Raccoon の公式ページでログインするとアカウントを追加できます。リモート画面で自動的に戻らない場合はコールバック URL を貼り付けてください。",
        "ko" => "Raccoon 공식 페이지에서 로그인하면 계정이 추가됩니다. 원격 패널에서 자동으로 돌아오지 않으면 콜백 URL을 붙여넣으세요.",
        "pt-BR" => "Conclua o login na página oficial do Raccoon para adicionar a conta; no acesso remoto ao painel, cole a URL de callback caso não retorne automaticamente.",
        _ => "在小浣熊官方页面完成登录即可添加账号；远程访问面板且未自动返回时，按提示粘贴回调地址即可。",
    }
}

pub fn token_instructions(locale: &str, json_mode: bool) -> &'static str {
    if json_mode {
        return match locale {
            "en" => "Select or paste a Raccoon credential file (such as auth.json) to import the account.",
            "zh-Hant" => "選擇或貼上小浣熊憑據檔案（如 auth.json）即可匯入帳號。",
            "ja" => "Raccoon の認証ファイル（auth.json など）を選択または貼り付けてアカウントを取り込めます。",
            "ko" => "Raccoon 인증 파일(auth.json 등)을 선택하거나 붙여넣어 계정을 가져올 수 있습니다.",
            "pt-BR" => "Selecione ou cole um arquivo de credenciais do Raccoon (como auth.json) para importar a conta.",
            _ => "选择或粘贴小浣熊凭据文件（如 auth.json）即可导入账号。",
        };
    }
    match locale {
        "en" => "Enter an Access Token to add the account; legacy credentials with a Refresh Token support automatic renewal.",
        "zh-Hant" => "填寫 Access Token 即可新增帳號；包含 Refresh Token 的舊版憑據支援自動續期。",
        "ja" => "Access Token を入力してアカウントを追加できます。Refresh Token を含む旧認証情報は自動更新に対応します。",
        "ko" => "Access Token을 입력해 계정을 추가할 수 있으며, Refresh Token이 포함된 기존 자격 증명은 자동 갱신을 지원합니다.",
        "pt-BR" => "Informe o Access Token para adicionar a conta; credenciais antigas com Refresh Token suportam renovação automática.",
        _ => "填写 Access Token 即可添加账号；包含 Refresh Token 的旧版凭据支持自动续期。",
    }
}

pub fn methods(locale: &str) -> Value {
    let desktop = sdk::login::desktop_method(3, "办公小浣熊 / Raccoon", locale);
    let labels = match locale {
        "en" => [
            "Browser sign-in",
            "Enter tokens",
            "Import auth JSON",
            "Credentials JSON",
        ],
        "zh-Hant" => ["網頁登入", "填寫令牌", "匯入登入態JSON", "登入態JSON"],
        "ja" => [
            "ブラウザーでログイン",
            "トークンを入力",
            "認証JSONを取り込む",
            "認証JSON",
        ],
        "ko" => [
            "브라우저 로그인",
            "토큰 입력",
            "인증JSON 가져오기",
            "인증JSON",
        ],
        "pt-BR" => [
            "Entrar pelo navegador",
            "Inserir tokens",
            "Importar JSON de login",
            "JSON de credenciais",
        ],
        _ => ["网页登录", "填写令牌", "导入登录态JSON", "登录态JSON"],
    };
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": labels[0], "instructions": browser_instructions(locale), "prompts": [], "browserMode": "choice",
            "manualCallback": true, "callbackInput": "url", "validatesCallback": true },
        { "methodIndex": 1, "type": "api", "label": labels[1], "instructions": token_instructions(locale, false), "prompts": [
            { "key": "accessToken", "label": "Access Token", "type": "text", "secret": true },
            { "key": "refreshToken", "label": "Refresh Token", "type": "text", "secret": true, "optional": true },
            { "key": "officeIdentity", "label": "Office Identity", "type": "text", "optional": true }
        ] },
        { "methodIndex": 2, "type": "api", "label": labels[2], "instructions": token_instructions(locale, true), "prompts": [
            { "key": "credentialJson", "label": labels[3], "type": "text", "secret": true }
        ] },
        desktop
    ] })
}
