//! Z.AI CLI 登录：不建本地回调，不捕获浏览器 Cookie；国内的 redirect 参数不能复用。
//! poll/OAuth/biz 凭据只在 flow 隔离状态，终态、取消与并发替换均销毁，不写日志。
use super::{
    account, config, login_key,
    network::{self, text, AUTHORIZE, BIZ, ZCODE},
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
        return Err(Error::http(400, "ZCode 国际版登录方式无效"));
    }
    if params["payload"]["methodIndex"].as_u64().unwrap_or(0) != 0 {
        return Err(Error::http(400, "手动凭据请通过账号表单提交"));
    }
    let flow = text(params, &["flow"]);
    if flow.is_empty() {
        return Err(Error::new("宿主未分配 ZCode 国际版登录流程"));
    }
    let version = config::app_version()?;
    let poll_token = network::random_hex(32)?;
    let generation = network::uuid()?;
    let now = sdk::now();
    let locale = super::i18n::locale(params["payload"]["locale"].as_str().unwrap_or("zh-CN"));
    let mut working = json!({ "pollToken": poll_token, "expiresAt": now + TTL, "locale": locale,
        "deviceMid": network::uuid()?, "phase": "initializing", "pollMs": 3000, "nextAt": now, "generation": generation });
    let before = state::get(Scope::Login, KEY, 1)?;
    if !state::compare_exchange(Scope::Login, KEY, 1, before.revision, working.clone(), TTL)? {
        return Err(Error::http(409, "ZCode 国际版登录流程已变化，请重新发起"));
    }
    let result = (|| -> Result<Value> {
        let reply = network::request(
            "POST",
            &format!("{ZCODE}/api/v1/oauth/cli/init"),
            json!({ "authorization": format!("Bearer {poll_token}"), "content-type": "application/json" }),
            Some(&json!({ "provider": "zai" })),
        )?;
        let data = network::envelope(reply, "ZCode 国际版登录发起")?;
        let upstream_flow = text(&data, &["flow_id"]);
        let authorize = text(&data, &["authorize_url"]);
        if upstream_flow.is_empty()
            || upstream_flow.len() > 256
            || matches!(upstream_flow.as_str(), "." | "..")
            || upstream_flow.chars().any(char::is_control)
            || authorize.is_empty()
            || authorize.len() > 8192
        {
            return Err(Error::http(502, "ZCode 国际版未返回完整登录流程"));
        }
        let mut url = url::Url::parse(&authorize)
            .map_err(|_| Error::http(502, "ZCode 国际版授权地址无效"))?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || ![AUTHORIZE, ZCODE, BIZ].contains(&url.origin().ascii_serialization().as_str())
        {
            return Err(Error::coded("ZCode 授权页超出国际版已知来源", "forbidden"));
        }
        let target = format!(
            "{ZCODE}/app/oauth/login?redirect=zcode%3A%2F%2Foauth%2Fcallback&app_version={version}"
        );
        let pairs = url
            .query_pairs()
            .filter(|(key, _)| key != "redirect_uri")
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        url.set_query(None);
        url.query_pairs_mut()
            .extend_pairs(pairs)
            .append_pair("redirect_uri", &target);
        let expires = network::integer(&data["expires_at"])
            .filter(|value| *value > 0)
            .map(|value| {
                if value > 100_000_000_000 {
                    value
                } else {
                    value.saturating_mul(1000)
                }
            })
            .unwrap_or(now + TTL)
            .min(now + TTL);
        let interval = network::integer(&data["poll_interval_sec"])
            .unwrap_or(3)
            .clamp(1, 60)
            * 1000;
        working["upstreamFlow"] = json!(upstream_flow);
        working["expiresAt"] = json!(expires);
        working["pollMs"] = json!(interval);
        working["phase"] = json!("pending");
        Ok(
            json!({ "flow": flow, "url": url.to_string(), "poll_interval_ms": interval,
            "instructions": super::i18n::text(locale, "authorize"),
            "external_browser": false, "allowed_hosts": ["zcode.z.ai", "chat.z.ai", "api.z.ai"],
            "login_ui": { "browserMode": "choice", "manualCallback": false, "callbackInput": "url", "cookieCapture": null, "captureCallback": false } }),
        )
    })();
    let current = state::get(Scope::Login, KEY, 1)?;
    if current.value["generation"] != generation || current.value["phase"] != "initializing" {
        return Err(Error::http(409, "ZCode 国际版登录已取消或被替换"));
    }
    match result {
        Ok(response) => {
            if !state::compare_exchange(Scope::Login, KEY, 1, current.revision, working, TTL)? {
                return Err(Error::http(409, "ZCode 国际版登录已取消或被替换"));
            }
            Ok(response)
        }
        Err(error) => {
            state::compare_exchange(Scope::Login, KEY, 1, current.revision, Value::Null, 1000)?;
            Err(error)
        }
    }
}

pub fn cancel() -> Result<Value> {
    for _ in 0..3 {
        let snapshot = state::get(Scope::Login, KEY, 1)?;
        if state::compare_exchange(Scope::Login, KEY, 1, snapshot.revision, Value::Null, 1000)? {
            return Ok(Value::Null);
        }
    }
    Err(Error::new("ZCode 国际版登录状态正在变化，请重试取消"))
}

pub fn poll(_params: &Value) -> Result<Value> {
    let mut snapshot = state::get(Scope::Login, KEY, 1)?;
    let locale = super::i18n::locale(snapshot.value["locale"].as_str().unwrap_or("zh-CN"));
    if !snapshot.compatible || !snapshot.value.is_object() {
        return Ok(failed(&super::i18n::text(locale, "cancelled")));
    }
    let now = sdk::now();
    if snapshot.value["expiresAt"].as_i64().unwrap_or(0) <= now {
        cancel()?;
        return Ok(failed(&super::i18n::text(locale, "expired")));
    }
    if snapshot.value["phase"] == "initializing"
        || snapshot.value["busyUntil"].as_i64().unwrap_or(0) > now
        || snapshot.value["nextAt"].as_i64().unwrap_or(0) > now
    {
        return Ok(pending(locale, &snapshot.value));
    }
    if snapshot.value["busyUntil"].as_i64().unwrap_or(0) > 0
        && snapshot.value["phase"] == "create-key"
    {
        // 前一次创建调用被取消或超时，不能在结果不明时再次 POST。
        cancel()?;
        return Ok(failed(
            "国际版编码密钥创建结果尚未确认，请重新登录并查询已有密钥",
        ));
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
    if snapshot.value["generation"] != generation || snapshot.value["lease"] != lease {
        return Ok(failed("ZCode 国际版登录已取消或被替换，未执行密钥换取"));
    }
    let result = step(&mut working);
    let current = state::get(Scope::Login, KEY, 1)?;
    if current.revision != snapshot.revision
        || current.value["generation"] != generation
        || current.value["lease"] != lease
    {
        return Ok(failed("ZCode 国际版登录已取消或被替换，未保存凭据"));
    }
    match result {
        Ok(Some(secret)) => {
            if sdk::now() >= current.value["expiresAt"].as_i64().unwrap_or(0) {
                state::compare_exchange(Scope::Login, KEY, 1, current.revision, Value::Null, 1000)?;
                return Ok(failed(&super::i18n::text(locale, "expired")));
            }
            let account = account::normalized(secret)?;
            if !state::compare_exchange(Scope::Login, KEY, 1, current.revision, Value::Null, 1000)?
            {
                return Ok(failed("ZCode 国际版登录已取消，未保存凭据"));
            }
            Ok(json!({ "state": "done", "account": account }))
        }
        Ok(None) => {
            working["busyUntil"] = json!(0);
            working["nextAt"] = json!(sdk::now() + working["pollMs"].as_i64().unwrap_or(3000));
            let next = working.clone();
            if !state::compare_exchange(Scope::Login, KEY, 1, current.revision, working, TTL)? {
                return Ok(failed("ZCode 国际版登录已取消"));
            }
            Ok(pending(locale, &next))
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
            json!({ "authorization": format!("Bearer {}", text(state, &["pollToken"])) }),
            None,
        ) {
            Ok(reply) => reply,
            Err(error) if error.code.as_deref() == Some("forbidden") => return Err(error),
            Err(_) => return Ok(None),
        };
        if reply.status >= 500 || matches!(reply.status, 408 | 429) {
            return Ok(None);
        }
        let status = reply.status;
        let value = reply.value;
        if (200..300).contains(&status) && !value.is_object() {
            return Ok(None);
        }
        if status >= 400 {
            return Err(Error::http(401, "ZCode 国际版登录轮询被拒，请重新发起"));
        }
        let code = network::integer(&value["code"]);
        if code.is_some_and(|code| code != 0) {
            return Err(Error::http(
                502,
                format!("ZCode 国际版登录被拒（{}）", code.unwrap_or(0)),
            ));
        }
        let data = &value["data"];
        let phase = data["status"].as_str().unwrap_or("");
        match phase {
            "pending" | "" => return Ok(None),
            "failed" | "denied" | "cancelled" | "canceled" | "expired" => {
                return Err(Error::http(401, "ZCode 国际版授权被拒、取消或已失效"))
            }
            "ready" => {}
            _ => return Err(Error::http(502, "ZCode 国际版返回未知授权终态，请重新登录")),
        }
        let oauth = text(&data["zai"], &["access_token", "accessToken"]);
        login_key::credential(&oauth)?;
        let jwt = text(data, &["token", "jwt"]);
        if !jwt.is_empty() {
            login_key::credential(&jwt)?;
        }
        state["oauth"] = json!(oauth);
        state["jwt"] = json!(jwt);
        state["userId"] = json!(account::user(&data["user"]));
        state["name"] = data["user"]["name"].clone();
        state["email"] = data["user"]["email"].clone();
        state["phase"] = json!("business-login");
        state["pollToken"] = Value::Null;
        return Ok(None);
    }
    login_key::advance(state)
}

fn failed(message: &str) -> Value {
    json!({ "state": "failed", "message": message })
}
fn pending(locale: &str, state: &Value) -> Value {
    json!({ "state": "pending", "hint": super::i18n::text(locale,
        if state["phase"] == "pending" || state["phase"] == "initializing" { "authorize" } else { "resolve_key" }) })
}

pub fn methods(locale: &str) -> Value {
    let labels = match locale {
        "en" => ["Browser sign-in", "Sign in to the international Z.AI edition. A complete coding API key is resolved after authorization.", "Enter credentials", "Import credentials JSON", "Credentials JSON", "Select or paste a ZCode International credential file to import the account."],
        "ja" => ["ブラウザーでログイン", "国際版 Z.AI にログインすると、認証後に完全なコーディング用 API Key を取得します。", "認証情報を入力", "認証情報 JSON を取り込む", "認証情報 JSON", "ZCode 国際版の認証ファイルを選択または貼り付けてアカウントを取り込めます。"],
        "ko" => ["브라우저 로그인", "국제판 Z.AI 인증 후 완전한 코딩 API Key를 가져옵니다.", "인증 정보 입력", "인증 정보 JSON 가져오기", "인증 정보 JSON", "ZCode 국제판 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있습니다."],
        "pt-BR" => ["Entrar pelo navegador", "Entre na edição internacional Z.AI. A chave completa de programação é obtida após a autorização.", "Inserir credenciais", "Importar credenciais JSON", "Credenciais JSON", "Selecione ou cole um arquivo de credenciais do ZCode International para importar a conta."],
        "zh-Hant" => ["網頁登入", "登入 ZCode 國際版（Z.AI），授權後自動取得完整編碼套餐密鑰。", "手動填寫憑證", "匯入憑證 JSON", "憑證 JSON", "選擇或貼上 ZCode 國際版憑據檔案即可匯入帳號。"],
        _ => ["网页登录", "登录 ZCode 国际版（Z.AI），授权后自动取得完整编码套餐密钥。", "手动填写凭据", "导入凭据 JSON", "凭据 JSON", "选择或粘贴 ZCode 国际版凭据文件即可导入账号。"],
    };
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": labels[0], "prompts": [], "browserMode": "choice", "manualCallback": false, "instructions": labels[1] },
        { "methodIndex": 1, "type": "api", "label": labels[2], "prompts": [
            { "key": "accessToken", "type": "text", "label": "Z.AI Access Token / API Key", "secret": true, "optional": true },
            { "key": "jwt", "type": "text", "label": "Start Plan JWT", "secret": true, "optional": true },
            { "key": "userId", "type": "text", "label": super::i18n::text(locale, "user"), "optional": true },
            { "key": "deviceMid", "type": "text", "label": super::i18n::text(locale, "device"), "optional": true },
            { "key": "zcodePlan", "type": "select", "label": super::i18n::text(locale, "plan"), "default": "coding-plan", "optional": true,
                "options": [{ "value": "coding-plan", "label": super::i18n::text(locale, "coding") }, { "value": "start-plan", "label": super::i18n::text(locale, "start") }] }
        ], "instructions": super::i18n::text(locale, "manual") },
        { "methodIndex": 2, "type": "api", "label": labels[3], "instructions": labels[5], "prompts": [
            { "key": "credentialJson", "type": "text", "label": labels[4], "secret": true }
        ] }
    ] })
}
