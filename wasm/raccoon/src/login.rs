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
        "en" => "Finish sign-in on the official Raccoon page. Desktop users can choose the embedded window or system browser. Web callbacks require an HTTP loopback panel address with an explicit port; forward the remote gateway port locally before signing in. Paste the full callback URL if automatic return fails.",
        "zh-Hant" => "請在小浣熊官方頁面完成登入；桌面可選內置視窗或系統瀏覽器。網頁回跳需 HTTP 回環面板位址及顯式連接埠，遠端部署請先將連接埠轉發到本機。未自動返回時可貼上完整回呼網址。",
        "ja" => "公式 Raccoon ページでログインしてください。デスクトップでは内蔵ウィンドウまたはシステムブラウザーを選べます。Web の戻り先には HTTP のループバックアドレスと明示的なポートが必要です。リモート接続では先にポートをローカルへ転送してください。自動で戻らない場合は完全なコールバック URL を貼り付けられます。",
        "ko" => "Raccoon 공식 페이지에서 로그인하세요. 데스크톱에서는 내장 창이나 시스템 브라우저를 선택할 수 있습니다. 웹 콜백에는 명시적 포트가 있는 HTTP 루프백 패널 주소가 필요합니다. 원격 연결에서는 먼저 포트를 로컬로 전달하세요. 자동 반환이 실패하면 전체 콜백 URL을 붙여넣을 수 있습니다.",
        "pt-BR" => "Conclua o login na página oficial do Raccoon. No desktop, escolha a janela integrada ou o navegador do sistema. O retorno web exige um endereço HTTP local com porta explícita; encaminhe a porta do gateway remoto para sua máquina antes de entrar. Se não retornar automaticamente, cole a URL completa de retorno.",
        _ => "请在小浣熊官方页面完成登录；桌面可选择内置窗口或系统浏览器。网页回跳需要 HTTP 回环面板地址和显式端口，远程部署请先将端口转发到本机。未自动返回时可粘贴完整回调地址。",
    }
}

pub fn desktop_instructions(locale: &str) -> &'static str {
    match locale {
        "en" => "Reads the official Raccoon login on the connected gateway computer. Refresh sign-in synchronizes the current login. Automatic renewal and writing rotated tokens back to the original file require separate permission in the plugin settings; IAM credentials require sign-in again.",
        "zh-Hant" => "讀取所連接閘道電腦上的小浣熊官方登入態。「重新整理登入態」可同步目前登入；自動續期及將新憑據寫回原檔案需在外掛權限中另外授權。IAM 憑據仍需重新登入。",
        "ja" => "接続先ゲートウェイのコンピューターにある Raccoon の公式ログイン情報を読み取ります。ログイン更新で現在の情報を同期できます。自動更新と元ファイルへの書き戻しはプラグイン設定で別途許可が必要です。IAM 認証情報は再ログインが必要です。",
        "ko" => "연결된 게이트웨이 컴퓨터의 공식 Raccoon 로그인 정보를 읽습니다. 로그인 새로고침으로 현재 정보를 동기화할 수 있습니다. 자동 갱신과 원본 파일에 새 자격 증명 저장은 플러그인 권한에서 별도로 허용해야 합니다. IAM 자격 증명은 다시 로그인해야 합니다.",
        "pt-BR" => "Lê o login oficial do Raccoon no computador do gateway conectado. Atualizar login sincroniza as credenciais atuais. A renovação automática e a gravação dos novos tokens no arquivo original exigem permissão separada nas configurações do plugin. Credenciais IAM exigem novo login.",
        _ => "读取所连接网关电脑上的小浣熊官方登录态。「刷新登录态」可同步当前登录；自动续期及将新凭据写回原文件需在插件权限中另外授权。IAM 凭据仍需重新登录。",
    }
}

pub fn methods(locale: &str) -> Value {
    let mut desktop = sdk::login::desktop_method(3, "办公小浣熊 / Raccoon", locale);
    desktop["instructions"] = json!(desktop_instructions(locale));
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
        { "methodIndex": 1, "type": "api", "label": labels[1], "prompts": [
            { "key": "accessToken", "label": "Access Token", "type": "text", "secret": true },
            { "key": "refreshToken", "label": "Refresh Token", "type": "text", "secret": true, "optional": true },
            { "key": "officeIdentity", "label": "Office Identity", "type": "text", "optional": true }
        ] },
        { "methodIndex": 2, "type": "api", "label": labels[2], "prompts": [
            { "key": "credentialJson", "label": labels[3], "type": "text", "secret": true }
        ] },
        desktop,
        sdk::legacy_credentials::method(4, locale)
    ] })
}
