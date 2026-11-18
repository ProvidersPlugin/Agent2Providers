//! CLI 轮询登录状态只存于宿主分配的 flow；终态与取消都销毁 poll/OAuth 凭据。
use super::{
    account, config, login_key,
    network::{self, text, ZCODE},
};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope},
    Error, Result, Value,
};

const KEY: &str = "cli-login";
const TTL: i64 = 300_000;

pub fn start(params: &Value) -> Result<Value> {
    if !matches!(
        params["method"].as_str().unwrap_or(""),
        "" | "web" | "browser"
    ) {
        return Err(Error::http(400, "ZCode 登录方式无效"));
    }
    if params["payload"]["methodIndex"].as_u64().unwrap_or(0) != 0 {
        return Err(Error::http(400, "手动凭据请通过账号表单提交"));
    }
    let flow = text(params, &["flow"]);
    if flow.is_empty() {
        return Err(Error::new("宿主未分配 ZCode 登录流程"));
    }
    let snapshot = state::get(Scope::Login, KEY, 1)?;
    let poll_token = network::random_hex(32)?;
    let device = network::uuid()?;
    let locale = params["payload"]["locale"]
        .as_str()
        .or_else(|| params["options"]["locale"].as_str())
        .unwrap_or("zh-CN");
    let reply = network::request(
        "POST",
        &format!("{ZCODE}/api/v1/oauth/cli/init"),
        json!({ "authorization": format!("Bearer {poll_token}"), "content-type": "application/json", "X-Device-Mid": device }),
        Some(&json!({ "provider": "bigmodel" })),
    )?;
    let data = network::envelope(reply, "ZCode 登录发起")?;
    let upstream_flow = text(&data, &["flow_id"]);
    let authorize = text(&data, &["authorize_url"]);
    if upstream_flow.is_empty() || upstream_flow.len() > 256 || authorize.is_empty() {
        return Err(Error::http(502, "ZCode 未返回完整登录流程"));
    }
    let mut url =
        url::Url::parse(&authorize).map_err(|_| Error::http(502, "ZCode 授权地址无效"))?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || !["https://bigmodel.cn", "https://open.bigmodel.cn", ZCODE]
            .contains(&url.origin().ascii_serialization().as_str())
    {
        return Err(Error::coded("ZCode 授权页超出国内版已知来源", "forbidden"));
    }
    let version = config::app_version()?;
    let target = format!(
        "{ZCODE}/app/oauth/login?redirect=zcode%3A%2F%2Foauth%2Fcallback&app_version={version}"
    );
    let pairs = url
        .query_pairs()
        .filter(|(key, _)| key != "redirect")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect::<Vec<_>>();
    url.set_query(None);
    url.query_pairs_mut()
        .extend_pairs(pairs)
        .append_pair("redirect", &target);
    let now = sdk::now();
    let expires = network::integer(&data["expires_at"])
        .filter(|value| *value > 0)
        .map(|value| value.saturating_mul(1000))
        .unwrap_or(now + TTL)
        .min(now + TTL);
    let interval = network::integer(&data["poll_interval_sec"])
        .unwrap_or(3)
        .clamp(1, 60)
        * 1000;
    let state = json!({ "upstreamFlow": upstream_flow, "pollToken": poll_token, "expiresAt": expires,
        "deviceMid": device, "locale": locale, "phase": "pending", "pollMs": interval, "nextAt": now, "generation": network::uuid()? });
    if !state::compare_exchange(Scope::Login, KEY, 1, snapshot.revision, state, TTL)? {
        return Err(Error::new("ZCode 登录流程已变化，请重新发起"));
    }
    Ok(
        json!({ "flow": flow, "url": url.to_string(), "poll_interval_ms": interval,
        "instructions": instructions(locale),
        "external_browser": true, "allowed_hosts": ["zcode.z.ai", "bigmodel.cn", "open.bigmodel.cn"],
        "login_ui": { "browserMode": "external", "manualCallback": false, "callbackInput": "url", "cookieCapture": null, "captureCallback": false } }),
    )
}

pub fn cancel() -> Result<Value> {
    for _ in 0..3 {
        let snapshot = state::get(Scope::Login, KEY, 1)?;
        if state::compare_exchange(Scope::Login, KEY, 1, snapshot.revision, Value::Null, 1000)? {
            return Ok(Value::Null);
        }
    }
    Err(Error::new("ZCode 登录状态正在变化，请重试取消"))
}

pub fn poll(_params: &Value) -> Result<Value> {
    let mut snapshot = state::get(Scope::Login, KEY, 1)?;
    if !snapshot.compatible || !snapshot.value.is_object() {
        return Ok(failed("ZCode 登录已取消或过期"));
    }
    let now = sdk::now();
    if snapshot.value["expiresAt"].as_i64().unwrap_or(0) <= now {
        cancel()?;
        return Ok(failed("ZCode 登录已过期，请重新发起"));
    }
    if snapshot.value["busyUntil"].as_i64().unwrap_or(0) > now
        || snapshot.value["nextAt"].as_i64().unwrap_or(0) > now
    {
        return Ok(pending(
            snapshot.value["locale"].as_str().unwrap_or("zh-CN"),
        ));
    }
    if snapshot.value["busyUntil"].as_i64().unwrap_or(0) > 0
        && snapshot.value["phase"] == "create-key"
    {
        // 前一次创建调用被取消或超时，不能在结果不明时再次 POST。
        cancel()?;
        return Ok(failed("编码密钥创建结果尚未确认，请重新登录并查询已有密钥"));
    }
    let generation = snapshot.value["generation"].clone();
    let lease = network::uuid()?;
    let mut working = snapshot.value.clone();
    working["busyUntil"] = json!(now + 120_000);
    working["lease"] = json!(lease);
    if !state::compare_exchange(
        Scope::Login,
        KEY,
        1,
        snapshot.revision,
        working.clone(),
        TTL,
    )? {
        return Ok(json!({ "state": "pending" }));
    }
    snapshot = state::get(Scope::Login, KEY, 1)?;
    let result = step(&mut working);
    let current = state::get(Scope::Login, KEY, 1)?;
    if current.revision != snapshot.revision
        || current.value["generation"] != generation
        || current.value["lease"] != lease
    {
        return Ok(failed("ZCode 登录已取消或被替换，未保存凭据"));
    }
    match result {
        Ok(Some(secret)) => {
            let account = account::normalized(secret)?;
            if !state::compare_exchange(Scope::Login, KEY, 1, current.revision, Value::Null, 1000)?
            {
                return Ok(failed("ZCode 登录已取消，未保存凭据"));
            }
            Ok(json!({ "state": "done", "account": account }))
        }
        Ok(None) => {
            working["busyUntil"] = json!(0);
            working["nextAt"] = json!(sdk::now() + working["pollMs"].as_i64().unwrap_or(3000));
            let locale = working["locale"].as_str().unwrap_or("zh-CN").to_string();
            if !state::compare_exchange(Scope::Login, KEY, 1, current.revision, working, TTL)? {
                return Ok(failed("ZCode 登录已取消"));
            }
            Ok(pending(&locale))
        }
        Err(error) => {
            state::compare_exchange(Scope::Login, KEY, 1, current.revision, Value::Null, 1000)?;
            if error.code.as_deref() == Some("forbidden") {
                return Err(error);
            }
            Ok(failed(&error.message))
        }
    }
}

fn step(state: &mut Value) -> Result<Option<Value>> {
    if state["phase"] == "pending" {
        let reply = match network::request(
            "GET",
            &format!(
                "{ZCODE}/api/v1/oauth/cli/poll/{}",
                network::query(&text(state, &["upstreamFlow"]))
            ),
            json!({ "authorization": format!("Bearer {}", text(state, &["pollToken"])), "X-Device-Mid": state["deviceMid"] }),
            None,
        ) {
            Ok(reply) => reply,
            Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
            Err(_) => return Ok(None),
        };
        if reply.status >= 500 || matches!(reply.status, 408 | 429) {
            return Ok(None);
        }
        if reply.status >= 400 {
            return Err(Error::http(401, "ZCode 登录轮询被拒，请重新发起"));
        }
        if !reply.value.is_object() {
            return Ok(None);
        }
        let code = network::integer(&reply.value["code"]).unwrap_or(0);
        if !matches!(code, 0 | 200) || reply.value["success"] == false {
            return Err(Error::http(502, format!("ZCode 登录被拒（{code}）")));
        }
        let data = &reply.value["data"];
        if data["status"] == "failed" {
            return Err(Error::http(401, "ZCode 授权被拒或已失效"));
        }
        if data["status"] != "ready" {
            return Ok(None);
        }
        let oauth =
            account::strip_bearer(&text(&data["bigmodel"], &["access_token", "accessToken"]));
        if oauth.is_empty() {
            return Err(Error::http(502, "ZCode 国内版登录结果缺少访问令牌"));
        }
        state["oauth"] = json!(oauth);
        state["jwt"] = json!(account::strip_bearer(&text(data, &["token"])));
        state["userId"] = json!(account::user(&data["user"]));
        state["name"] = data["user"]["name"].clone();
        state["email"] = data["user"]["email"].clone();
        state["phase"] = json!("organization");
        state["pollToken"] = Value::Null;
        return Ok(None);
    }
    login_key::advance(state)
}

fn failed(message: &str) -> Value {
    json!({ "state": "failed", "message": message })
}

fn instructions(locale: &str) -> &'static str {
    match locale {
        "en" => "Complete sign-in on the official China-edition page. Authorization is polled automatically; no callback needs to be pasted.",
        "ja" => "公式の中国版ページでログインしてください。認証結果は自動で確認され、コールバックの貼り付けは不要です。",
        "ko" => "공식 중국판 페이지에서 로그인하세요. 인증 결과를 자동으로 확인하므로 콜백을 붙여 넣을 필요가 없습니다.",
        "pt-BR" => "Conclua o login na página oficial da edição China. A autorização é consultada automaticamente, sem colar o retorno.",
        "zh-Hant" => "請在官方國內版頁面登入，授權結果會自動輪詢，無須貼上回呼地址。",
        _ => "请在官方国内版页面登录，授权结果会自动轮询，无需粘贴回调地址。",
    }
}

fn pending(locale: &str) -> Value {
    json!({ "state": "pending", "hint": instructions(locale) })
}

pub fn methods(locale: &str) -> Value {
    let labels = match locale {
        "en" => [
            "Browser sign-in",
            "Sign in to the China edition. The coding API key is resolved after authorization.",
            "Enter credentials",
            "Import credentials JSON",
            "Credentials JSON",
        ],
        "ja" => [
            "ブラウザーでログイン",
            "中国版にログインすると、認証後にコーディング用の API Key を取得します。",
            "認証情報を入力",
            "認証情報 JSON を取り込む",
            "認証情報 JSON",
        ],
        "ko" => [
            "브라우저 로그인",
            "중국판에서 인증하면 코딩 API Key를 가져옵니다.",
            "인증 정보 입력",
            "인증 정보 JSON 가져오기",
            "인증 정보 JSON",
        ],
        "pt-BR" => [
            "Entrar pelo navegador",
            "Entre na edição China. A chave de programação é obtida após a autorização.",
            "Inserir credenciais",
            "Importar credenciais JSON",
            "Credenciais JSON",
        ],
        "zh-Hant" => [
            "網頁登入",
            "登入 ZCode 國內版，授權後自動取得編碼套餐密鑰。",
            "手動填寫憑證",
            "匯入憑證 JSON",
            "憑證 JSON",
        ],
        _ => [
            "网页登录",
            "登录 ZCode 国内版，授权后自动取得编码套餐密钥。",
            "手动填写凭据",
            "导入凭据 JSON",
            "凭据 JSON",
        ],
    };
    let fields = match locale {
        "en" => ["User ID", "Device UUID", "Plan channel", "Coding Plan", "Start Plan", "Choose the matching credential: API key for Coding Plan, JWT for Start Plan. User ID may be omitted for existing credentials; such records are deduplicated by credentials, not a stable user identity."],
        "ja" => ["ユーザー ID", "デバイス UUID", "プラン経路", "Coding Plan", "Start Plan", "Coding Plan は API Key、Start Plan は JWT を使用します。既存の認証情報はユーザー ID を省略できますが、固定ユーザー ID ではなく認証情報で重複を判定します。"],
        "ko" => ["사용자 ID", "기기 UUID", "요금제 경로", "Coding Plan", "Start Plan", "Coding Plan은 API Key, Start Plan은 JWT를 사용합니다. 기존 인증 정보의 사용자 ID는 생략할 수 있으며, 이 경우 고정 사용자 신원이 아닌 인증 정보로 중복을 판별합니다."],
        "pt-BR" => ["ID do usuário", "UUID do dispositivo", "Canal do plano", "Coding Plan", "Start Plan", "Use a chave API para Coding Plan e JWT para Start Plan. O ID pode ser omitido em credenciais existentes; nesse caso a deduplicação usa as credenciais, não uma identidade estável."],
        "zh-Hant" => ["使用者 ID", "裝置 UUID", "套餐通道", "編碼套餐", "套餐池", "編碼套餐使用 API Key，套餐池使用 JWT。舊憑證可不填使用者 ID，但此類記錄按憑證去重，沒有固定使用者身分。"],
        _ => ["用户 ID", "设备 UUID", "套餐通道", "编码套餐", "套餐池", "编码套餐使用 API Key，套餐池使用 JWT。旧凭据可不填用户 ID，但此类记录按凭据去重，没有固定用户身份。"],
    };
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": labels[0], "prompts": [], "browserMode": "external", "manualCallback": false, "instructions": labels[1] },
        { "methodIndex": 1, "type": "api", "label": labels[2], "instructions": fields[5], "prompts": [
            { "key": "accessToken", "type": "text", "label": "Access Token / API Key", "secret": true, "optional": true },
            { "key": "jwt", "type": "text", "label": "Start Plan JWT", "secret": true, "optional": true },
            { "key": "userId", "type": "text", "label": fields[0], "optional": true },
            { "key": "deviceMid", "type": "text", "label": fields[1], "optional": true },
            { "key": "zcodePlan", "type": "select", "label": fields[2], "optional": true, "default": "coding-plan",
                "options": [{ "value": "coding-plan", "label": fields[3] }, { "value": "start-plan", "label": fields[4] }] }
        ] },
        { "methodIndex": 2, "type": "api", "label": labels[3], "prompts": [
            { "key": "credentialJson", "type": "text", "label": labels[4], "secret": true }
        ] }
    ] })
}
