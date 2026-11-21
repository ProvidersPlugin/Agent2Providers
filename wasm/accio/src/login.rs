//! 宿主一次性 flow + PKCE；只复用已登记的 /auth/callback-accio，不开本机监听器。
//! code/verifier 不进公开返回、错误或账号；取消/超时用 flow 隔离状态清理。

use crate::{account, credentials::Credentials, endpoints, errors, network, util};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope, Snapshot},
    Error, Result, Value,
};
use url::Url;

const KEY: &str = "accio.oauth";
const TTL: i64 = 360_000;

pub fn methods() -> Value {
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": "Accio 国际版网页登录", "browserMode": "choice",
            "manualCallback": true, "callbackInput": "url", "validatesCallback": true,
            "instructions": "在 www.accio.com 完成登录即可添加账号；远程访问面板时，按提示粘贴登录后的回调地址即可。",
            "prompts": [{ "key": "name", "type": "text", "label": "备注名", "optional": true }] },
        { "methodIndex": 1, "type": "api", "label": "手工填写凭据",
            "instructions": "填写国际版访问令牌；填写刷新令牌后支持自动续期。",
            "prompts": [
                { "key": "accessToken", "type": "text", "label": "访问令牌", "secret": true },
                { "key": "refreshToken", "type": "text", "label": "刷新令牌", "optional": true, "secret": true },
                { "key": "name", "type": "text", "label": "备注名", "optional": true }
            ] },
        { "methodIndex": 2, "type": "api", "label": "导入凭据 JSON",
            "instructions": "选择或粘贴 Accio 国际版凭据文件即可导入账号；包含完整登录信息时支持自动续期。",
            "prompts": [
                { "key": "credentialJson", "type": "text", "label": "凭据 JSON", "secret": true },
                { "key": "name", "type": "text", "label": "备注名", "optional": true }
            ] }
    ] })
}

pub fn methods_for(locale: &str) -> Value {
    let mut value = methods();
    let rows = [
        (["Accio 国际版网页登录", "Accio international web sign-in", "Accio 國際版網頁登入", "Accio 国際版のウェブログイン", "Accio 국제판 웹 로그인", "Login web do Accio internacional"],
         ["在 www.accio.com 完成登录即可添加账号；远程访问面板时，按提示粘贴登录后的回调地址即可。", "Sign in at www.accio.com to add the account; when accessing the panel remotely, paste the callback URL after signing in.", "在 www.accio.com 完成登入即可新增帳號；遠端存取面板時，按提示貼上登入後的回呼網址即可。", "www.accio.com でログインするとアカウントを追加できます。リモート画面ではログイン後にコールバック URL を貼り付けてください。", "www.accio.com에서 로그인하면 계정이 추가됩니다. 원격 패널에서는 로그인 후 콜백 URL을 붙여넣으세요.", "Conclua o login em www.accio.com para adicionar a conta; no acesso remoto ao painel, cole a URL de callback após o login."]),
        (["手工填写凭据", "Enter credentials", "手動填寫憑據", "認証情報を入力", "인증 정보 입력", "Inserir credenciais"],
         ["填写国际版访问令牌；填写刷新令牌后支持自动续期。", "Enter an international access token; include a refresh token to enable automatic renewal.", "填寫國際版存取權杖；填寫更新權杖後支援自動續期。", "国際版のアクセストークンを入力してください。更新トークンを入力すると自動更新に対応します。", "국제판 액세스 토큰을 입력하세요. 갱신 토큰을 함께 입력하면 자동 갱신을 지원합니다.", "Insira o token de acesso internacional; informe o token de renovação para ativar a renovação automática."]),
        (["导入凭据 JSON", "Import credential JSON", "匯入憑據 JSON", "認証情報 JSON を取り込む", "인증 JSON 가져오기", "Importar JSON de credenciais"],
         ["选择或粘贴 Accio 国际版凭据文件即可导入账号；包含完整登录信息时支持自动续期。", "Select or paste an Accio international credential file to import the account; automatic renewal is supported when complete sign-in details are included.", "選擇或貼上 Accio 國際版憑據檔案即可匯入帳號；包含完整登入資訊時支援自動續期。", "Accio 国際版の認証ファイルを選択または貼り付けてアカウントを取り込めます。完全なログイン情報が含まれていれば自動更新に対応します。", "Accio 국제판 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있으며, 전체 로그인 정보가 포함되어 있으면 자동 갱신을 지원합니다.", "Selecione ou cole um arquivo de credenciais do Accio internacional para importar a conta; a renovação automática é suportada quando os dados completos de login estão incluídos."]),
    ];
    for (row, (labels, instructions)) in value["methods"]
        .as_array_mut()
        .into_iter()
        .flatten()
        .zip(rows)
    {
        row["label"] = json!(localized(locale, labels));
        row["instructions"] = json!(localized(locale, instructions));
        for prompt in row["prompts"].as_array_mut().into_iter().flatten() {
            let labels = match prompt["key"].as_str().unwrap_or("") {
                "name" => [
                    "备注名",
                    "Nickname",
                    "備註名稱",
                    "表示名",
                    "별칭",
                    "Apelido",
                ],
                "accessToken" => [
                    "访问令牌",
                    "Access token",
                    "存取權杖",
                    "アクセストークン",
                    "액세스 토큰",
                    "Token de acesso",
                ],
                "refreshToken" => [
                    "刷新令牌",
                    "Refresh token",
                    "更新權杖",
                    "更新トークン",
                    "갱신 토큰",
                    "Token de renovação",
                ],
                _ => [
                    "凭据 JSON",
                    "Credential JSON",
                    "憑據 JSON",
                    "認証情報 JSON",
                    "인증 JSON",
                    "JSON de credenciais",
                ],
            };
            prompt["label"] = json!(localized(locale, labels));
        }
    }
    value
}

fn localized(locale: &str, values: [&str; 6]) -> String {
    values[match locale {
        "en" => 1,
        "zh-Hant" => 2,
        "ja" => 3,
        "ko" => 4,
        "pt-BR" => 5,
        _ => 0,
    }]
    .into()
}

fn callback_instructions(locale: &str) -> String {
    if endpoints::ID == "accio-cn" {
        return localized(locale, [
            "在 Accio 国内站完成授权；远程部署请粘贴本次完整回调地址。",
            "Authorize on Accio mainland; remotely, paste this login's complete callback URL.",
            "在 Accio 國內站完成授權；遠端部署請貼上本次完整回呼位址。",
            "Accio 中国国内版で認証し、リモートの場合は今回の完全なコールバック URL を貼り付けてください。",
            "Accio 중국 국내판에서 인증하고, 원격에서는 이번 로그인의 전체 콜백 URL을 붙여넣으세요.",
            "Autorize no Accio da China; remotamente, cole a URL completa de retorno deste login.",
        ]);
    }
    if endpoints::ID != "accio" {
        return "等待 Accio 国内版授权回调".into();
    }
    localized(locale, ["在 Accio 国际站完成授权；远程部署请粘贴本次完整回调地址。", "Authorize on Accio international; remotely, paste the complete callback URL for this login.", "在 Accio 國際站完成授權；遠端部署請貼上本次完整回呼位址。", "Accio 国際版で認証し、リモートの場合は今回の完全なコールバック URL を貼り付けてください。", "Accio 국제판에서 인증하고, 원격인 경우 이번 로그인의 전체 콜백 URL을 붙여넣으세요.", "Autorize no Accio internacional; remotamente, cole a URL completa de retorno deste login."])
}

fn flow(params: &Value) -> Result<&str> {
    params["flow"]
        .as_str()
        .filter(|flow| {
            !flow.is_empty()
                && flow.len() <= 256
                && flow
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        })
        .ok_or_else(|| Error::http(503, "主程序未提供有效的 Accio 登录流程"))
}

fn parsed(raw: &str) -> Result<Url> {
    if raw.is_empty()
        || raw.len() > 32 * 1024
        || raw
            .bytes()
            .any(|byte| byte <= b' ' || byte == 127 || byte == b'\\')
    {
        return Err(Error::http(400, "Accio 回调地址格式无效"));
    }
    let url = Url::parse(raw).map_err(|_| Error::http(400, "Accio 回调地址格式无效"))?;
    let authority = raw
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""))
        .unwrap_or("");
    if authority.is_empty()
        || authority.contains('@')
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::http(400, "Accio 回调不能包含用户信息或片段"));
    }
    Ok(url)
}

fn redirect(params: &Value) -> Result<String> {
    // 只从宿主可信根参数读，绝不接受表单里的 redirectUri/callback_url。
    let mut url = parsed(params["callback_url"].as_str().unwrap_or(""))?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none_or(|port| port == 0)
        || url.query().is_some()
    {
        return Err(Error::http(503, "主程序未提供可信本机回调地址"));
    }
    url.set_path(endpoints::CALLBACK);
    Ok(url.to_string())
}

fn replace(snapshot: &Snapshot, value: Value, ttl: i64) -> Result<()> {
    if !state::compare_exchange(Scope::Login, KEY, 1, snapshot.revision, value, ttl)? {
        return Err(Error::http(409, "Accio 授权状态已变化，请勿重复提交"));
    }
    Ok(())
}

fn active(snapshot: &Snapshot, now: i64) -> bool {
    let created = snapshot.value["created_at"].as_i64().unwrap_or(0);
    snapshot.compatible
        && created > 0
        && (0..TTL).contains(&now.saturating_sub(created))
        && matches!(
            snapshot.value["phase"].as_str(),
            Some("pending" | "exchanging")
        )
}

pub fn start(params: &Value) -> Result<Value> {
    if let Some(hint) = params["method"].as_str() {
        if hint != "browser" {
            return Err(Error::http(400, "Accio 网页登录方式无效"));
        }
    }
    let flow = flow(params)?;
    let payload = &params["payload"];
    let locale = payload["locale"].as_str().unwrap_or("zh-CN");
    if payload
        .get("methodIndex")
        .is_some_and(|index| index.as_u64() != Some(0))
    {
        return Err(Error::http(400, "凭据导入请通过账号表单提交"));
    }
    let fields = payload.get("inputs").cloned().unwrap_or_else(|| json!({}));
    let fields = fields
        .as_object()
        .ok_or_else(|| Error::http(400, "Accio 登录字段必须是对象"))?;
    if fields
        .iter()
        .any(|(key, value)| key != "name" || !value.is_string())
    {
        return Err(Error::http(400, "Accio 登录包含未申报字段"));
    }
    let name: String = fields
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(100)
        .collect();
    let redirect = redirect(params)?;
    let verifier = util::verifier()?;
    let challenge = util::challenge(&verifier);
    let snapshot = state::get(Scope::Login, KEY, 1)?;
    if !snapshot.compatible || !snapshot.value.is_null() {
        return Err(Error::http(409, "Accio 登录流程已存在，请重新发起"));
    }
    replace(
        &snapshot,
        json!({ "phase": "pending", "created_at": util::now()?, "state": flow,
        "verifier": verifier, "redirect_uri": redirect, "edition": endpoints::EDITION, "name": name, "locale": locale }),
        TTL,
    )?;
    let url = format!(
        "{}/login?return_url={}&state={}&code_challenge={}&code_challenge_method=S256&client_id={}",
        endpoints::LOGIN,
        util::query(&redirect),
        util::query(flow),
        util::query(&challenge),
        endpoints::CLIENT_ID
    );
    Ok(json!({ "flow": flow, "url": url, "poll_interval_ms": 0,
        "instructions": callback_instructions(locale),
        // 可选择临时内嵌窗口或系统浏览器；第三方页面导航不赋予插件网络来源权限。
        "external_browser": false, "allowed_hosts": [],
        "login_ui": { "browserMode": "choice", "manualCallback": true, "callbackInput": "url",
            "captureCallback": true, "cookieCapture": null, "loopbackTargets": [redirect] } }))
}

struct Callback {
    code: String,
    denied: bool,
}

fn callback_code(params: &Value, state: &Value) -> Result<Callback> {
    let raw = params["url"].as_str().unwrap_or("");
    let url = parsed(raw)?;
    let expected = parsed(state["redirect_uri"].as_str().unwrap_or(""))?;
    if url.scheme() != expected.scheme()
        || url.host_str() != expected.host_str()
        || url.port_or_known_default() != expected.port_or_known_default()
        || url.path() != expected.path()
        || url.path() != endpoints::CALLBACK
        || raw.split('?').next() != Some(expected.as_str())
    {
        return Err(Error::http(400, "Accio 回调不属于本次本机接收地址"));
    }
    let pairs: Vec<_> = url.query_pairs().collect();
    let states: Vec<_> = pairs.iter().filter(|(key, _)| key == "state").collect();
    if states.len() != 1
        || states[0].1 != flow(params)?
        || state["state"] != flow(params)?
        || state["edition"] != endpoints::EDITION
    {
        return Err(Error::http(400, "Accio 回调与本次国际版授权不匹配"));
    }
    let errors: Vec<_> = pairs.iter().filter(|(key, _)| key == "error").collect();
    let codes: Vec<_> = pairs.iter().filter(|(key, _)| key == "code").collect();
    if !errors.is_empty() {
        if errors.len() != 1
            || errors[0].1.trim().is_empty()
            || errors[0].1.len() > 512
            || errors[0].1.chars().any(char::is_control)
        {
            return Err(Error::http(400, "Accio 回调错误参数无效"));
        }
        // 官方拒绝优先于同时回显的旧 code；只结束原流程，绝不兑换任何材料。
        return Ok(Callback {
            code: String::new(),
            denied: true,
        });
    }
    if codes.len() != 1
        || codes[0].1.is_empty()
        || codes[0].1.len() > 8192
        || codes[0].1.chars().any(char::is_control)
        || codes[0].1.trim() != codes[0].1.as_ref()
        || pairs.iter().any(|(key, _)| key == "authorization_code")
    {
        return Err(Error::http(400, "Accio 回调必须包含唯一有效授权码"));
    }
    if params["code"]
        .as_str()
        .is_some_and(|code| !code.is_empty() && code != codes[0].1.as_ref())
    {
        return Err(Error::http(400, "Accio 回调授权码与请求不一致"));
    }
    Ok(Callback {
        code: codes[0].1.to_string(),
        denied: false,
    })
}

pub fn callback(params: &Value) -> Result<Value> {
    flow(params)?;
    let snapshot = state::get(Scope::Login, KEY, 1)?;
    if !active(&snapshot, util::now()?) {
        replace(&snapshot, Value::Null, 1000)?;
        return Ok(json!({ "state": "failed", "message": "Accio 登录已过期或取消" }));
    }
    if snapshot.value["phase"] != "pending" {
        return Err(Error::http(409, "Accio 授权正在处理，请勿重复提交"));
    }
    let received = match callback_code(params, &snapshot.value) {
        Ok(received) => received,
        Err(error) => {
            return Ok(json!({ "state": "pending", "retryable": true, "hint": error.message }))
        }
    };
    if received.denied {
        replace(&snapshot, Value::Null, 1000)?;
        return Ok(json!({ "state": "failed", "message": "Accio 授权已取消或被拒绝" }));
    }
    let attempt = util::uuid()?;
    // verifier 只留在这次调用栈内；提交前从 flow 状态移除，防止重复换码。
    replace(
        &snapshot,
        json!({ "phase": "exchanging", "created_at": snapshot.value["created_at"], "attempt": attempt,
            "locale": snapshot.value["locale"] }),
        snapshot.value["created_at"]
            .as_i64()
            .unwrap_or(0)
            .saturating_add(TTL)
            .saturating_sub(util::now()?)
            .max(1000),
    )?;
    let result = (|| -> Result<Value> {
        let verifier = snapshot.value["verifier"]
            .as_str()
            .filter(|value| (43..=128).contains(&value.len()))
            .ok_or_else(|| Error::http(400, "Accio 授权材料已失效，请重新发起"))?;
        let body = json!({ "code": received.code, "codeVerifier": verifier, "clientId": endpoints::CLIENT_ID,
            "redirectUri": snapshot.value["redirect_uri"] });
        let data = network::data(network::post(
            endpoints::TOKEN,
            &body,
            &[&received.code, verifier],
        )?);
        let mut credential = Credentials::parse(&data)?;
        match network::profile(&credential) {
            Ok(profile) => credential.profile(&profile),
            Err(error) if errors::host_stop(&error) => return Err(error),
            Err(_) => {}
        }
        credential.ensure_device(true)?;
        account::normalized(&credential, "web", snapshot.value["name"].as_str())
    })();
    let current = state::get(Scope::Login, KEY, 1)?;
    if !active(&current, util::now()?)
        || current.value["phase"] != "exchanging"
        || current.value["attempt"] != attempt
    {
        return Err(Error::http(409, "Accio 登录已取消或过期，未保存迟到的账号"));
    }
    replace(&current, Value::Null, 1000)?;
    match result {
        Ok(account) => Ok(sdk::login::done(account)),
        Err(error) => Ok(json!({ "state": "failed", "message": error.message })),
    }
}

pub fn poll(params: &Value) -> Result<Value> {
    flow(params)?;
    let snapshot = state::get(Scope::Login, KEY, 1)?;
    if active(&snapshot, util::now()?) {
        let locale = snapshot.value["locale"].as_str().unwrap_or("zh-CN");
        return Ok(json!({ "state": "pending", "hint": callback_instructions(locale) }));
    }
    replace(&snapshot, Value::Null, 1000)?;
    Ok(json!({ "state": "failed", "message": "Accio 登录已过期或取消，请重新发起" }))
}

pub fn cancel(params: &Value) -> Result<Value> {
    flow(params)?;
    let snapshot = state::get(Scope::Login, KEY, 1)?;
    replace(&snapshot, Value::Null, 1000)?;
    Ok(Value::Null)
}
