//! 云端回调轮询：宿主 flow 与上游 state 分离，PKCE/DPoP 只在 Scope::Login 保存。
//! 每次 poll 只推进一个阶段，实例销毁不丢状态；一次性换码/初始化中断不自动重放。
//! 未给模型凭据时要求额外明确确认；不自动接受服务条款、购买套餐或开启续费。

use super::{
    account, client, credentials::Credentials, dpop::DpopKey, endpoints, errors, login_methods,
    oauth_gateway, oauth_tokens, util,
};
use agent2api_wasm_sdk::{
    self as sdk, json,
    login::{Field, Interaction, OptionItem},
    state::{self, Scope, Snapshot},
    Error, Result, Value,
};
use sha2::{Digest, Sha256};

const KEY: &str = "oauth-login";
const TTL: i64 = 10 * 60 * 1000;
const INTERVAL: i64 = 2500;
const MAX_INTERVAL: i64 = 60_000;

fn flow(params: &Value) -> Result<&str> {
    params["flow"]
        .as_str()
        .filter(|flow| !flow.is_empty() && flow.len() <= 256 && !flow.contains('\0'))
        .ok_or_else(|| Error::http(400, "主程序未提供可信 OfficeAce 登录流程标识"))
}
fn load() -> Result<Snapshot> {
    let state = state::get(Scope::Login, KEY, 1)?;
    if !state.compatible {
        return Err(Error::http(409, "OfficeAce 登录状态格式已变化，请重新发起"));
    }
    Ok(state)
}
fn save(old: &Snapshot, value: Value) -> Result<bool> {
    let ttl = value["expiresAt"]
        .as_i64()
        .map(|at| at.saturating_sub(sdk::now()).clamp(1000, TTL))
        .unwrap_or(1000);
    state::compare_exchange(Scope::Login, KEY, 1, old.revision, value, ttl)
}
fn failed(message: &str) -> Value {
    json!({ "state": "failed", "message": message })
}
fn pending() -> Value {
    sdk::login::pending(None)
}

pub fn start(params: &Value) -> Result<Value> {
    let flow = flow(params)?;
    if params["method"]
        .as_str()
        .is_some_and(|method| !matches!(method, "" | "browser"))
        || params["payload"]["methodIndex"].as_u64().unwrap_or(0) != 0
    {
        return Err(Error::http(
            400,
            "OfficeAce 请通过网页登录获取授权，手动凭据使用导入入口",
        ));
    }
    let pair = DpopKey::generate()?;
    let verifier = util::b64url(&util::random(32)?);
    let challenge = util::b64url(&Sha256::digest(verifier.as_bytes()));
    let reply = client::plain(
        "POST",
        &format!("{}/v1/claw/auth/state", endpoints::CLOUD),
        Some(&json!({})),
    )?;
    if !(200..300).contains(&reply.status) {
        return Err(Error::http(
            502,
            format!("果办云端未能创建授权状态（HTTP {}）", reply.status),
        ));
    }
    let upstream = reply.value["state"]
        .as_str()
        .map(str::trim)
        .filter(|state| !state.is_empty() && state.len() <= 1024)
        .ok_or_else(|| Error::http(502, "果办云端没有返回有效授权状态"))?;
    let inner = format!("{}/authui/v1/oauth2/authorize?client_id={}&code_challenge={}&code_challenge_method=SHA-256&state={}&scope=openid&redirect_uri={}&response_type=code",
        endpoints::AUTH, endpoints::CLIENT_ID, util::encode(&challenge), util::encode(upstream), util::encode(endpoints::CALLBACK));
    let url = format!(
        "{}/authui/login.html?service={}",
        endpoints::AUTH,
        util::encode(&inner)
    );
    let now = sdk::now();
    let name = params["payload"]["name"]
        .as_str()
        .unwrap_or("")
        .trim()
        .chars()
        .take(100)
        .collect::<String>();
    let old = load()?;
    if old.value.is_object() {
        return Err(Error::http(
            409,
            "OfficeAce 登录流程已经开始，请使用本次授权页面",
        ));
    }
    let locale = params["payload"]["locale"].as_str().unwrap_or("zh-CN");
    let instructions = login_methods::methods(locale)["methods"][0]["instructions"].clone();
    if !save(
        &old,
        json!({ "phase": "waiting", "upstreamState": upstream, "verifier": verifier,
        "pair": pair, "name": name, "locale": locale, "intervalMs": INTERVAL,
        "expiresAt": now.saturating_add(TTL), "nextAt": now }),
    )? {
        return Err(Error::http(409, "OfficeAce 登录流程已变化，请重新发起"));
    }
    Ok(
        json!({ "flow": flow, "url": url, "poll_interval_ms": INTERVAL,
        "instructions": instructions,
        "external_browser": true, "allowed_hosts": ["auth.huaweicloud.com"],
        "login_ui": { "browserMode": "external", "manualCallback": false, "callbackInput": "url", "cookieCapture": null, "captureCallback": false } }),
    )
}

fn confirmation(value: &Value) -> Value {
    let (title, message, label, decline, confirm) = match value["locale"].as_str().unwrap_or("zh-CN") {
        "en" => ("Initialize OfficeAce model access", "The service has not returned model credentials. This only requests model access; it does not accept terms, buy a plan or enable automatic renewal. Plans require separate confirmation.", "Initialize model access?", "Not now", "Confirm model access"),
        "ja" => ("OfficeAce モデル利用を初期化", "公式サービスがモデル認証情報を返していません。モデル利用の申請のみ行い、利用規約への同意、プランの購入、自動更新は行いません。プランは別途確認が必要です。", "モデル利用を初期化しますか", "今は行わない", "モデル利用の初期化を確認"),
        "ko" => ("OfficeAce 모델 접근 초기화", "서비스가 모델 인증 정보를 반환하지 않았습니다. 모델 접근만 요청하며 약관 동의, 요금제 구매 또는 자동 갱신은 하지 않습니다. 요금제는 별도 확인이 필요합니다.", "모델 접근을 초기화할까요", "나중에", "모델 접근 초기화 확인"),
        "pt-BR" => ("Inicializar acesso ao modelo OfficeAce", "O serviço ainda não retornou credenciais do modelo. Esta ação só solicita acesso; não aceita termos, compra planos nem ativa renovação automática. Planos exigem confirmação separada.", "Inicializar acesso ao modelo?", "Agora não", "Confirmar acesso ao modelo"),
        "zh-Hant" => ("確認初始化果辦模型存取", "官方尚未回傳模型憑證。此步驟只申請模型存取，不會同意服務條款、購買套餐或開啟自動續費。套餐仍需另行確認。", "是否初始化模型存取", "暫不初始化", "確認初始化模型存取"),
        _ => ("确认初始化果办模型访问", "官方尚未返回模型访问凭据。此步骤只申请模型访问，不会确认服务条款、购买体验套餐或开启自动续费。套餐仍需在签到中心另行确认。", "是否初始化模型访问", "暂不初始化", "确认初始化模型访问"),
    };
    sdk::login::pending(Some(Interaction {
        id: value["interactionId"].as_str().unwrap_or("").into(),
        title: title.into(),
        message: message.into(),
        fields: vec![Field {
            key: "confirmModelAccess".into(),
            label: label.into(),
            kind: "select".into(),
            secret: false,
            optional: false,
            options: vec![
                OptionItem {
                    value: "decline".into(),
                    label: decline.into(),
                },
                OptionItem {
                    value: "confirm".into(),
                    label: confirm.into(),
                },
            ],
        }],
        captcha: None,
    }))
}

pub fn submit(params: &Value) -> Result<Value> {
    flow(params)?;
    let old = load()?;
    if old.value["phase"] != "consent"
        || old.value["expiresAt"].as_i64().unwrap_or(0) <= sdk::now()
        || old.value["interactionId"] != params["interaction"]
    {
        return Err(Error::http(409, "OfficeAce 登录确认步骤已变化"));
    }
    let value = &params["inputs"]["confirmModelAccess"];
    if value == "decline" {
        cancel(params)?;
        return Ok(failed("已取消模型访问初始化，请先在官方客户端完成后再登录"));
    }
    if value != "confirm" {
        return Err(Error::http(400, "请明确确认或取消模型访问初始化"));
    }
    let mut next = old.value.clone();
    next["phase"] = json!("binding");
    next["confirmedModelAccess"] = json!(true);
    next["nextAt"] = json!(0);
    if !save(&old, next)? {
        return Err(Error::http(409, "OfficeAce 登录状态已变化"));
    }
    Ok(pending())
}

pub fn poll(params: &Value) -> Result<Value> {
    flow(params)?;
    let old = load()?;
    if old.value["phase"] == "done" {
        return Ok(sdk::login::done(old.value["account"].clone()));
    }
    if old.value["phase"] == "failed" {
        return Ok(failed(
            old.value["message"]
                .as_str()
                .unwrap_or("OfficeAce 登录失败"),
        ));
    }
    let now = sdk::now();
    if !old.value.is_object()
        || old.value["expiresAt"].as_i64().unwrap_or(0) <= now
        || old.value["phase"] == "cancelled"
    {
        return Ok(failed("OfficeAce 登录已取消或过期，请重新发起"));
    }
    if old.value["phase"] == "consent" {
        return Ok(confirmation(&old.value));
    }
    let busy = old.value["busyUntil"].as_i64().unwrap_or(0);
    if busy > now || old.value["nextAt"].as_i64().unwrap_or(0) > now {
        return Ok(pending());
    }
    if busy > 0 && matches!(old.value["phase"].as_str(), Some("exchange" | "binding")) {
        save(
            &old,
            json!({ "phase": "failed", "message": "一次性授权或初始化结果未确认，未自动重复提交，请重新登录", "expiresAt": old.value["expiresAt"] }),
        )?;
        return Ok(failed(
            "一次性授权或初始化结果未确认，未自动重复提交，请重新登录",
        ));
    }
    let lease = util::random_hex(16)?;
    let mut working = old.value.clone();
    working["lease"] = json!(lease);
    working["busyUntil"] = json!(now.saturating_add(120_000));
    if !save(&old, working.clone())? {
        return Ok(pending());
    }
    let reserved = load()?;
    if reserved.value["lease"] != lease {
        return Ok(failed("OfficeAce 登录已取消或更新"));
    }
    let result = advance(&mut working);
    let current = load()?;
    if current.revision != reserved.revision || current.value["lease"] != lease {
        return Ok(failed("OfficeAce 登录已取消或更新，未保存返回的凭据"));
    }
    match result {
        Ok(Some(account)) => {
            if !save(
                &reserved,
                json!({ "phase": "done", "account": account, "expiresAt": working["expiresAt"] }),
            )? {
                return Ok(failed("OfficeAce 登录已取消，未添加账号"));
            }
            Ok(sdk::login::done(account))
        }
        Ok(None) => {
            working["busyUntil"] = json!(0);
            let interval = working["intervalMs"]
                .as_i64()
                .unwrap_or(INTERVAL)
                .clamp(INTERVAL, MAX_INTERVAL);
            working["nextAt"] = json!(sdk::now().saturating_add(interval));
            if !save(&reserved, working.clone())? {
                return Ok(failed("OfficeAce 登录已取消"));
            }
            if working["phase"] == "consent" {
                Ok(confirmation(&working))
            } else {
                Ok(pending())
            }
        }
        Err(mut error) => {
            errors::redact(&mut error, &working);
            save(
                &reserved,
                json!({ "phase": "failed", "message": error.message, "expiresAt": working["expiresAt"] }),
            )?;
            Ok(failed(&error.message))
        }
    }
}

fn backoff(working: &mut Value) {
    let current = working["intervalMs"]
        .as_i64()
        .unwrap_or(INTERVAL)
        .max(INTERVAL);
    working["intervalMs"] = json!(current.saturating_mul(2).min(MAX_INTERVAL));
}

fn advance(working: &mut Value) -> Result<Option<Value>> {
    match working["phase"].as_str().unwrap_or("") {
        "waiting" => {
            let url = format!(
                "{}/v1/claw/auth/code?state={}",
                endpoints::CLOUD,
                util::encode(working["upstreamState"].as_str().unwrap_or(""))
            );
            let response = match client::open_raw(
                "GET",
                &url,
                json!({ "accept": "application/json" }),
                None,
            ) {
                Ok(response) => response,
                Err(error) if errors::permission(&error) => return Err(error),
                Err(_) => {
                    backoff(working);
                    return Ok(None);
                }
            };
            if matches!(response.status, 429 | 503) {
                backoff(working);
                return Ok(None);
            }
            if response.status == 404 {
                return Err(Error::http(400, "授权状态已失效，请重新网页登录"));
            }
            let status = response.status;
            let reply = match client::read(response, false) {
                Ok(reply) => reply,
                Err(_) if status >= 500 || (200..300).contains(&status) => {
                    backoff(working);
                    return Ok(None);
                }
                Err(error) => return Err(error),
            };
            let code = util::text(&reply.value, &["error_code"]);
            let detail = util::text(&reply.value, &["error_msg", "error_message", "message"]);
            if code == "AgentArts.1100011"
                || detail.to_ascii_lowercase().contains("too many requests")
                || reply.status >= 500
                || reply.status == 408
            {
                backoff(working);
                return Ok(None);
            }
            working["intervalMs"] = json!(INTERVAL);
            if reply.status == 202
                || code == "AgentArts.11000009"
                || detail.to_ascii_lowercase().contains("not ready")
            {
                return Ok(None);
            }
            if !(200..300).contains(&reply.status)
                || reply
                    .value
                    .get("error")
                    .is_some_and(|value| !value.is_null())
                || !matches!(code.as_str(), "" | "0" | "200")
            {
                return Err(Error::http(
                    400,
                    format!("果办云端拒绝授权取码（HTTP {}）", reply.status),
                ));
            }
            let code = util::text(&reply.value, &["code", "authorization_code"]);
            if code.is_empty() || code.len() > 8192 {
                return Err(Error::http(502, "云端未返回有效授权码"));
            }
            working["code"] = json!(code);
            working["phase"] = json!("exchange");
        }
        "exchange" => {
            let mut secret = oauth_tokens::exchange(
                working["code"].as_str().unwrap_or(""),
                working["verifier"].as_str().unwrap_or(""),
                &working["pair"],
            )?;
            secret["dpopKeyPair"] = working["pair"].clone();
            working["secret"] = secret;
            if let Some(fields) = working.as_object_mut() {
                for key in ["pair", "verifier", "code", "upstreamState"] {
                    fields.remove(key);
                }
            }
            working["phase"] = json!("project");
        }
        "project" => {
            if util::text(&working["secret"], &["projectId"]).is_empty() {
                if let Ok(project) =
                    oauth_gateway::project(&Credentials::from_secret(&working["secret"]))
                {
                    working["secret"]["projectId"] = json!(project);
                }
            }
            working["phase"] = json!("permission");
        }
        "permission" => {
            let permission =
                oauth_gateway::permission(&Credentials::from_secret(&working["secret"]))?;
            let claimed = util::text(&working["secret"], &["principalId"]);
            if !claimed.is_empty()
                && !permission.principal.is_empty()
                && permission.principal != claimed
            {
                return Err(Error::http(
                    409,
                    "果办授权身份与权限校验身份不一致，未添加账号",
                ));
            }
            working["secret"]["accountId"] = json!(permission.account);
            if claimed.is_empty() {
                working["secret"]["principalId"] = json!(permission.principal);
            }
            if oauth_gateway::has_model(&permission.model) {
                oauth_gateway::apply_model(&mut working["secret"], &permission.model)?;
                working["phase"] = json!("complete");
            } else {
                working["phase"] = json!("consent");
                working["interactionId"] = json!(util::random_hex(16)?);
            }
        }
        "binding" => {
            if working["confirmedModelAccess"] != true {
                return Err(Error::http(400, "未确认模型访问初始化"));
            }
            let model = oauth_gateway::bind_model(&Credentials::from_secret(&working["secret"]))?;
            oauth_gateway::apply_model(&mut working["secret"], &model)?;
            working["phase"] = json!("complete");
        }
        "complete" => {
            return account::normalized(working["secret"].clone(), working["name"].as_str())
                .map(Some)
        }
        _ => return Err(Error::http(409, "OfficeAce 登录阶段无效，请重新发起")),
    }
    Ok(None)
}

pub fn cancel(params: &Value) -> Result<Value> {
    flow(params)?;
    for _ in 0..3 {
        let old = load()?;
        if save(
            &old,
            json!({ "phase": "cancelled", "expiresAt": sdk::now().saturating_add(1000) }),
        )? {
            return Ok(Value::Null);
        }
    }
    Err(Error::http(
        409,
        "OfficeAce 登录状态正在变化，未覆盖其他流程",
    ))
}
