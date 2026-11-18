//! 国内登录由官方auth/state与auth/token轮询收尾，不增加callback要求。
//! 宿主flow隔离Scope::Login；官方state和取得的token只放私有登录状态，CAS阻止取消后复活。
use super::{account, endpoints, headers, network, util};
use agent2api_wasm_sdk::{
    self as sdk, json,
    state::{self, Scope, Snapshot},
    Error, Result, Value,
};

const KEY: &str = "browser-login";
const TTL: i64 = 600_000;
fn flow(params: &Value) -> Result<&str> {
    params["flow"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        .ok_or_else(|| Error::http(400, "主程序未提供可信WorkBuddy登录流程"))
}
fn load() -> Result<Snapshot> {
    let state = state::get(Scope::Login, KEY, 1)?;
    if !state.compatible {
        return Err(Error::http(409, "登录状态格式已变化，请重新发起"));
    }
    Ok(state)
}
fn save(snapshot: &Snapshot, value: Value) -> Result<bool> {
    let ttl = value["until"]
        .as_i64()
        .map(|at| at.saturating_sub(sdk::now()).clamp(1000, TTL))
        .unwrap_or(1000);
    state::compare_exchange(Scope::Login, KEY, 1, snapshot.revision, value, ttl)
}
pub fn methods(locale: &str) -> Value {
    let labels = match locale {
        "en" => [
            "Browser sign-in",
            "Import credentials JSON",
            "Credentials JSON",
        ],
        "zh-Hant" => ["網頁登入", "匯入登入態JSON", "登入態JSON"],
        "ja" => ["ブラウザーでログイン", "認証JSONを取り込む", "認証JSON"],
        "ko" => ["브라우저 로그인", "인증JSON 가져오기", "인증JSON"],
        "pt-BR" => [
            "Entrar pelo navegador",
            "Importar JSON de login",
            "JSON de credenciais",
        ],
        _ => ["网页登录", "导入登录态JSON", "登录态JSON"],
    };
    let (browser_hint, desktop_hint) = match locale {
        "en" => ("Complete sign-in on the official page. The gateway continues automatically; no callback needs to be pasted.", "Imports a one-time credential snapshot. Encrypted WorkBuddy 5.6+ sign-in files cannot be imported; use browser sign-in instead."),
        "zh-Hant" => ("在官方頁面完成登入後會自動繼續，不需貼上回呼。", "匯入一次性登入快照；WorkBuddy 5.6 及以後的加密登入檔案無法直接匯入，請改用網頁登入。"),
        "ja" => ("公式ページでログインすると自動的に続行します。コールバックの貼り付けは不要です。", "認証情報を一度だけ取り込みます。WorkBuddy 5.6 以降の暗号化されたログインファイルは取り込めません。ブラウザーでログインしてください。"),
        "ko" => ("공식 페이지에서 로그인하면 자동으로 계속됩니다. 콜백 주소를 붙여 넣을 필요가 없습니다.", "로그인 정보를 한 번만 가져옵니다. WorkBuddy 5.6 이후의 암호화된 파일은 가져올 수 없으니 브라우저 로그인을 사용하세요."),
        "pt-BR" => ("Conclua o login na página oficial. O gateway continua automaticamente; não é preciso colar um retorno.", "Importa uma cópia das credenciais uma única vez. Arquivos criptografados do WorkBuddy 5.6 ou posterior não podem ser importados; use o login pelo navegador."),
        _ => ("在官方页面完成登录后会自动继续，不需要粘贴回调。", "导入一次性登录快照；WorkBuddy 5.6 及以后的加密登录文件无法直接导入，请改用网页登录。"),
    };
    let mut desktop = sdk::login::desktop_method(2, "WorkBuddy", locale);
    let general = desktop["instructions"].as_str().unwrap_or("");
    desktop["instructions"] = json!(format!("{general}\n{desktop_hint}"));
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": labels[0], "prompts": [], "browserMode": "choice", "manualCallback": false, "instructions": browser_hint },
        { "methodIndex": 1, "type": "api", "label": labels[1], "instructions": desktop_hint, "prompts": [{ "key": "credentialJson", "label": labels[2], "type": "text", "secret": true }] },
        desktop,
        sdk::legacy_credentials::method(3, locale)
    ] })
}
fn allowed(url: &str) -> bool {
    url::Url::parse(url).ok().is_some_and(|url| {
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port_or_known_default() == Some(443)
            && url.host_str().is_some_and(|host| {
                [
                    "workbuddy.cn",
                    "codebuddy.cn",
                    "tencent.com",
                    "qq.com",
                    "wechat.com",
                    "tenpay.com",
                ]
                .iter()
                .any(|known| host == *known || host.ends_with(&format!(".{known}")))
            })
    })
}
pub fn start(params: &Value) -> Result<Value> {
    let flow = flow(params)?;
    if params["method"]
        .as_str()
        .is_some_and(|s| !matches!(s, "" | "browser" | "web"))
        || params["payload"]["methodIndex"].as_u64().unwrap_or(0) != 0
    {
        return Err(Error::http(400, "请选择WorkBuddy网页登录入口"));
    }
    let previous = load()?;
    if previous.value.is_object() {
        return Err(Error::http(409, "WorkBuddy登录已发起，请使用当前页面"));
    }
    let until = sdk::now().saturating_add(TTL);
    if !save(&previous, json!({ "phase": "starting", "until": until }))? {
        return Err(Error::http(409, "登录流程已变化"));
    }
    let reserved = load()?;
    if reserved.value["phase"] != "starting" || reserved.value["until"] != until {
        return Err(Error::http(409, "WorkBuddy登录已取消或更新"));
    }
    let data = network::request(
        "POST",
        endpoints::ORIGIN,
        "/v2/plugin/auth/state?platform=workbuddy",
        headers::login(true)?,
        Some(&json!({})),
        &[],
    )?;
    let state = util::text(&data, &["state", "authState"]);
    let url = util::text(&data, &["authUrl"]);
    if state.is_empty()
        || state.len() > 4096
        || state.chars().any(char::is_control)
        || !allowed(&url)
    {
        return Err(Error::http(502, "WorkBuddy未返回有效的官方授权地址"));
    }
    if !save(
        &reserved,
        json!({ "phase": "waiting", "state": state, "until": until, "nextPoll": sdk::now() }),
    )? {
        return Err(Error::http(409, "登录已取消或更新"));
    }
    Ok(
        json!({ "flow": flow, "url": url, "poll_interval_ms": 3000, "external_browser": false,
        "instructions": "在官方页面完成登录，主程序会自动继续；不需要回填回调。",
        "allowed_hosts": ["workbuddy.cn", "codebuddy.cn", "tencent.com", "qq.com", "wechat.com", "tenpay.com"],
        "login_ui": { "browserMode": "choice", "manualCallback": false, "callbackInput": "url", "cookieCapture": null, "captureCallback": false } }),
    )
}
fn visible(value: &Value) -> Value {
    match value["phase"].as_str() {
        Some("done") => sdk::login::done(value["account"].clone()),
        Some("cancelled") => json!({ "state": "failed", "message": "登录已取消" }),
        Some("failed") => json!({ "state": "failed", "message": value["message"] }),
        _ => json!({ "state": "pending", "hint": "等待官方登录或账号资料就绪" }),
    }
}
pub fn poll(params: &Value) -> Result<Value> {
    flow(params)?;
    let previous = load()?;
    if matches!(
        previous.value["phase"].as_str(),
        Some("done" | "failed" | "cancelled")
    ) {
        return Ok(visible(&previous.value));
    }
    if previous.value["until"].as_i64().unwrap_or(0) <= sdk::now() {
        return Ok(json!({ "state": "failed", "message": "登录已过期，请重新发起" }));
    }
    if !matches!(
        previous.value["phase"].as_str(),
        Some("waiting" | "profile")
    ) {
        return Ok(visible(&previous.value));
    }
    if previous.value["nextPoll"].as_i64().unwrap_or(0) > sdk::now() {
        return Ok(visible(&previous.value));
    }
    let mut reserved = previous.value.clone();
    reserved["nextPoll"] = json!(sdk::now() + 3000);
    let reservation = util::random_hex(16)?;
    reserved["pollReservation"] = json!(reservation);
    if !save(&previous, reserved)? {
        return Ok(visible(&load()?.value));
    }
    let previous = load()?;
    if previous.value["pollReservation"] != reservation
        || !matches!(
            previous.value["phase"].as_str(),
            Some("waiting" | "profile")
        )
    {
        return Ok(visible(&previous.value));
    }
    let mut next = previous.value.clone();
    if !next["auth"].is_object() {
        let state = util::text(&next, &["state"]);
        let reply = match network::raw(
            "GET",
            endpoints::ORIGIN,
            &format!("/v2/plugin/auth/token?state={}", endpoints::encode(&state)),
            headers::login(false)?,
            None,
        ) {
            Ok(reply) => reply,
            Err(_) => {
                return Ok(
                    json!({ "state": "pending", "hint": "官方登录查询暂未完成，将继续等待" }),
                )
            }
        };
        if reply.code() == Some(11217) {
            return Ok(visible(&next));
        }
        let retryable = matches!(reply.status, 408 | 429) || reply.status >= 500;
        let data = match reply.checked(&[&state]) {
            Ok(data) => data,
            Err(_) if retryable => return Ok(visible(&next)),
            Err(error) => {
                let failed =
                    json!({ "phase": "failed", "until": next["until"], "message": error.message });
                if !save(&previous, failed.clone())? {
                    return Ok(visible(&load()?.value));
                }
                return Ok(visible(&failed));
            }
        };
        if util::text(&data, &["accessToken"]).is_empty() {
            return Ok(visible(&next));
        }
        let mut auth = data;
        for (absolute, relative) in [
            ("expiresAt", "expiresIn"),
            ("refreshExpiresAt", "refreshExpiresIn"),
        ] {
            if let Some(at) = util::expires(&auth, absolute, relative, sdk::now()) {
                auth[absolute] = json!(at);
            }
        }
        next["phase"] = json!("profile");
        next["auth"] = auth;
        if !save(&previous, next.clone())? {
            return Ok(visible(&load()?.value));
        }
    }
    let previous = load()?;
    if previous.value["phase"] != "profile" || previous.value["pollReservation"] != reservation {
        return Ok(visible(&previous.value));
    }
    let auth = &previous.value["auth"];
    let token = util::text(auth, &["accessToken"]);
    let expected = util::claims(&token)
        .map(|claims| util::text(&claims, &["sub", "userId", "uid"]))
        .unwrap_or_default();
    let profile = network::raw_timeout(
        "GET",
        endpoints::ORIGIN,
        "/v2/plugin/account",
        headers::login_account(auth)?,
        None,
        4000,
    );
    let attempts = previous.value["profileAttempts"].as_u64().unwrap_or(0);
    let mut profile = match profile {
        Ok(reply) if reply.code() == Some(12151) && attempts < 5 => {
            let mut next = previous.value.clone();
            next["profileAttempts"] = json!(attempts + 1);
            save(&previous, next.clone())?;
            return Ok(visible(&next));
        }
        Ok(reply) => reply.checked(&[&token]).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };
    let mut list = Value::Null;
    if util::text(&profile, &["uid"]).is_empty() {
        list = network::raw_timeout(
            "GET",
            endpoints::ORIGIN,
            "/v2/plugin/accounts",
            headers::login_account(auth)?,
            None,
            4000,
        )
        .and_then(|reply| reply.checked(&[&token]))
        .unwrap_or(Value::Null);
        let rows: Vec<_> = list["accounts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|row| expected.is_empty() || row["uid"] == expected)
            .collect();
        let last: Vec<_> = rows
            .iter()
            .copied()
            .filter(|row| match &row["lastLogin"] {
                Value::Bool(value) => *value,
                Value::String(value) => !value.trim().is_empty(),
                Value::Number(value) => value.as_f64().is_some_and(|value| value > 0.0),
                _ => false,
            })
            .collect();
        profile = if last.len() == 1 {
            last[0].clone()
        } else if rows.len() == 1 {
            rows[0].clone()
        } else {
            Value::Null
        };
    }
    if util::text(&profile, &["uid"]).is_empty() && !expected.is_empty() {
        let claims = util::claims(&token).unwrap_or(Value::Null);
        profile = json!({ "uid": expected, "nickname": util::text(&claims, &["nickname", "name", "preferred_username", "email"]) });
    }
    let profile_type = util::text(&profile, &["type"]);
    let enterprise = util::text(&profile, &["enterpriseId"]);
    if !enterprise.is_empty() && util::text(&profile, &["enterpriseName"]).is_empty()
        || enterprise.is_empty()
            && !profile_type.is_empty()
            && !profile_type.eq_ignore_ascii_case("personal")
    {
        if list.is_null() {
            list = network::raw_timeout(
                "GET",
                endpoints::ORIGIN,
                "/v2/plugin/accounts",
                headers::login_account(auth)?,
                None,
                4000,
            )
            .and_then(|reply| reply.checked(&[&token]))
            .unwrap_or(Value::Null);
        }
        let uid = util::text(&profile, &["uid"]);
        let rows: Vec<_> = list["accounts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|row| {
                util::text(row, &["uid"]) == uid
                    && !util::text(row, &["enterpriseId"]).is_empty()
                    && (enterprise.is_empty() || util::text(row, &["enterpriseId"]) == enterprise)
            })
            .collect();
        let typed: Vec<_> = rows
            .iter()
            .copied()
            .filter(|row| util::text(row, &["type"]).eq_ignore_ascii_case(&profile_type))
            .collect();
        let selected = if rows.len() == 1 {
            Some(rows[0])
        } else if enterprise.is_empty() && typed.len() == 1 {
            Some(typed[0])
        } else {
            None
        };
        if let Some(selected) = selected {
            for key in [
                "enterpriseId",
                "enterpriseName",
                "departmentFullName",
                "oneidAccountId",
                "idp",
                "type",
            ] {
                let value = util::text(selected, &[key]);
                if util::text(&profile, &[key]).is_empty() && !value.is_empty() {
                    profile[key] = json!(value);
                }
            }
        }
        if enterprise.is_empty() && util::text(&profile, &["enterpriseId"]).is_empty() {
            let failed = json!({ "phase": "failed", "until": previous.value["until"], "message": "官方已选择企业账号，但所属企业无法唯一确认；请重新登录或导入完整登录态" });
            if !save(&previous, failed.clone())? {
                return Ok(visible(&load()?.value));
            }
            return Ok(visible(&failed));
        }
    }
    let uid = util::text(&profile, &["uid"]);
    let result = if uid.is_empty() || !expected.is_empty() && uid != expected {
        json!({ "phase": "failed", "until": previous.value["until"], "message": "官方登录成功但账号身份无法确认，请重新发起" })
    } else {
        match account::parse(&json!({ "account": profile, "auth": auth }), None)
            .and_then(account::bundle)
        {
            Ok(account) => {
                json!({ "phase": "done", "until": previous.value["until"], "account": account })
            }
            Err(error) => {
                json!({ "phase": "failed", "until": previous.value["until"], "message": error.message })
            }
        }
    };
    if !save(&previous, result.clone())? {
        return Ok(visible(&load()?.value));
    }
    Ok(visible(&result))
}
pub fn cancel(params: &Value) -> Result<Value> {
    flow(params)?;
    for _ in 0..3 {
        let previous = load()?;
        if save(
            &previous,
            json!({ "phase": "cancelled", "until": sdk::now().saturating_add(TTL) }),
        )? {
            return Ok(Value::Null);
        }
    }
    Err(Error::http(409, "登录状态正在变化，请重试取消"))
}
