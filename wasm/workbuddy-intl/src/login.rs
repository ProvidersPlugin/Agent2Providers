//! 国际官方授权轮询；auth/state→auth/token→login/account?state=→accounts→JWT sub。
//! 登录state和新凭据仅在宿主Login私有scope，轮询抢占用CAS，取消不能复活。
use crate::{account, endpoints, headers, network, util};
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
        .ok_or_else(|| Error::http(400, "主程序未提供可信的国际版登录流程"))
}
fn load() -> Result<Snapshot> {
    let value = state::get(Scope::Login, KEY, 1)?;
    if !value.compatible {
        return Err(Error::http(409, "国际版登录状态格式变化，请重新发起"));
    }
    Ok(value)
}
fn save(before: &Snapshot, value: Value) -> Result<bool> {
    let ttl = value["until"]
        .as_i64()
        .map(|at| at.saturating_sub(sdk::now()).clamp(1000, TTL))
        .unwrap_or(1000);
    state::compare_exchange(Scope::Login, KEY, 1, before.revision, value, ttl)
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
        _ => ["网页登录", "导入国际登录态JSON", "登录态JSON"],
    };
    let (browser_hint, file_hint) = match locale {
        "en" => ("Complete authorization on the official international site; the account will be added automatically.", "Select or paste a WorkBuddy International credential file (such as .info) to import the account."),
        "zh-Hant" => ("在官方國際站完成授權即可自動新增帳號，無需貼上回呼碼。", "選擇或貼上 WorkBuddy 國際版憑據檔案（如 .info）即可匯入帳號。"),
        "ja" => ("公式の国際サイトで認証を完了すると自動でアカウントが追加されます。", "WorkBuddy 国際版の認証ファイル（.info など）を選択または貼り付けてアカウントを取り込めます。"),
        "ko" => ("공식 국제 사이트에서 인증을 완료하면 계정이 자동으로 추가됩니다.", "WorkBuddy 국제판 인증 파일(.info 등)을 선택하거나 붙여넣어 계정을 가져올 수 있습니다."),
        "pt-BR" => ("Conclua a autorização no site internacional oficial para adicionar a conta automaticamente.", "Selecione ou cole um arquivo de credenciais do WorkBuddy International (como .info) para importar a conta."),
        _ => ("在官方国际站完成授权即可自动添加账号，无需回填授权码。", "选择或粘贴 WorkBuddy 国际版凭据文件（如 .info）即可导入账号。"),
    };
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": labels[0], "prompts": [], "browserMode": "choice", "manualCallback": false, "instructions": browser_hint },
        { "methodIndex": 1, "type": "api", "label": labels[1], "instructions": file_hint, "prompts": [{ "key": "credentialJson", "type": "text", "secret": true, "label": labels[2] }] },
        sdk::login::desktop_method(2, "WorkBuddy International", locale)
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
                    "workbuddy.ai",
                    "codebuddy.ai",
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
        return Err(Error::http(400, "请选择国际版官方网页登录入口"));
    }
    let before = load()?;
    if before.value.is_object() {
        return Err(Error::http(409, "国际版登录已经发起，请使用当前流程"));
    }
    let until = sdk::now().saturating_add(TTL);
    if !save(&before, json!({ "phase": "starting", "until": until }))? {
        return Err(Error::http(409, "登录流程已变化"));
    }
    let reserved = load()?;
    if reserved.value["phase"] != "starting" || reserved.value["until"] != until {
        return Err(Error::http(409, "登录已取消或更新"));
    }
    let data = network::raw_timeout(
        "POST",
        endpoints::ORIGIN,
        "/v2/plugin/auth/state?platform=workbuddy-ai",
        headers::login(true)?,
        Some(&json!({})),
        20_000,
    )?
    .checked(&[])?;
    let state = util::text(&data, &["state", "authState"]);
    let url = util::text(&data, &["authUrl"]);
    if state.is_empty()
        || state.len() > 4096
        || state.chars().any(char::is_control)
        || !allowed(&url)
    {
        return Err(Error::http(502, "国际站未返回有效的官方授权地址"));
    }
    if !save(
        &reserved,
        json!({ "phase": "waiting", "state": state, "until": until, "nextPoll": sdk::now() }),
    )? {
        return Err(Error::http(409, "登录已取消或更新"));
    }
    Ok(
        json!({ "flow": flow, "url": url, "poll_interval_ms": 3000, "external_browser": false,
        "instructions": "在国际站完成官方授权，主程序将自动轮询，不需要回填授权码。",
        "allowed_hosts": ["workbuddy.ai", "codebuddy.ai", "tencent.com", "qq.com", "wechat.com", "tenpay.com"],
        "login_ui": { "browserMode": "choice", "manualCallback": false, "callbackInput": "url", "cookieCapture": null, "captureCallback": false } }),
    )
}
fn visible(value: &Value) -> Value {
    match value["phase"].as_str() {
        Some("done") => sdk::login::done(value["account"].clone()),
        Some("cancelled") => json!({ "state": "failed", "message": "登录已取消" }),
        Some("failed") => json!({ "state": "failed", "message": value["message"] }),
        _ => json!({ "state": "pending", "hint": "等待国际站授权或账号资料就绪" }),
    }
}
struct PollLease {
    ticket: String,
}
impl Drop for PollLease {
    fn drop(&mut self) {
        if let Ok(before) = load() {
            if before.value["pollLease"] == self.ticket
                && matches!(before.value["phase"].as_str(), Some("waiting" | "profile"))
            {
                let mut next = before.value.clone();
                next["busyUntil"] = json!(0);
                next["pollLease"] = Value::Null;
                next["nextPoll"] = json!(sdk::now().saturating_add(3000));
                let _ = save(&before, next);
            }
        }
    }
}
fn poll_time(value: &Value, maximum: u64) -> Result<u64> {
    let remaining = value["until"]
        .as_i64()
        .unwrap_or(0)
        .saturating_sub(sdk::now());
    if remaining < 1000 {
        return Err(Error::http(410, "国际版登录已过期，请重新发起"));
    }
    Ok((remaining as u64).min(maximum))
}
fn poll_current(before: &Snapshot, ticket: &str) -> Result<Option<Value>> {
    let latest = load()?;
    if latest.revision != before.revision || latest.value["pollLease"] != ticket {
        return Ok(Some(visible(&latest.value)));
    }
    if latest.value["until"].as_i64().unwrap_or(0) <= sdk::now() {
        return Ok(Some(
            json!({ "state": "failed", "message": "国际版登录已过期，请重新发起" }),
        ));
    }
    Ok(None)
}
pub fn poll(params: &Value) -> Result<Value> {
    flow(params)?;
    let before = load()?;
    if matches!(
        before.value["phase"].as_str(),
        Some("done" | "failed" | "cancelled")
    ) {
        return Ok(visible(&before.value));
    }
    if before.value["until"].as_i64().unwrap_or(0) <= sdk::now() {
        return Ok(json!({ "state": "failed", "message": "国际版登录已过期，请重新发起" }));
    }
    if !matches!(before.value["phase"].as_str(), Some("waiting" | "profile"))
        || before.value["nextPoll"].as_i64().unwrap_or(0) > sdk::now()
        || before.value["busyUntil"].as_i64().unwrap_or(0) > sdk::now()
    {
        return Ok(visible(&before.value));
    }
    let ticket = util::random_hex(16)?;
    let mut reserved = before.value.clone();
    reserved["nextPoll"] = json!(sdk::now().saturating_add(3000));
    reserved["busyUntil"] = json!(sdk::now().saturating_add(125_000));
    reserved["pollLease"] = json!(ticket);
    if !save(&before, reserved)? {
        return Ok(visible(&load()?.value));
    }
    let _lease = PollLease { ticket };
    let before = load()?;
    if before.value["pollLease"] != _lease.ticket
        || !matches!(before.value["phase"].as_str(), Some("waiting" | "profile"))
    {
        return Ok(visible(&before.value));
    }
    if !before.value["auth"].is_object() {
        let state = util::text(&before.value, &["state"]);
        let reply = match network::raw_timeout(
            "GET",
            endpoints::ORIGIN,
            &format!("/v2/plugin/auth/token?state={}", endpoints::encode(&state)),
            headers::login(false)?,
            None,
            poll_time(&before.value, 20_000)?,
        ) {
            Ok(reply) => reply,
            Err(_) => return Ok(visible(&before.value)),
        };
        if let Some(result) = poll_current(&before, &_lease.ticket)? {
            return Ok(result);
        }
        if reply.code() == Some(11217) {
            return Ok(visible(&before.value));
        }
        let mut auth = match reply.checked(&[&state]) {
            Ok(auth) if !util::text(&auth, &["accessToken"]).is_empty() => auth,
            _ => return Ok(visible(&before.value)),
        };
        // 先用与导入一致的令牌格式校验，再让新凭据进入私有轮询状态。
        let token = util::strip(&util::text(&auth, &["accessToken"]));
        if token.len() < 8 || token.len() > 32_768 || token.chars().any(char::is_whitespace) {
            let failed = json!({ "phase": "failed", "until": before.value["until"], "message": "国际站返回的访问凭据无效，请重新登录" });
            if !save(&before, failed.clone())? {
                return Ok(visible(&load()?.value));
            }
            return Ok(visible(&failed));
        }
        auth["accessToken"] = json!(token);
        for (absolute, relative) in [
            ("expiresAt", "expiresIn"),
            ("refreshExpiresAt", "refreshExpiresIn"),
        ] {
            if let Some(at) = util::expires(&auth, absolute, relative, sdk::now()) {
                auth[absolute] = json!(at);
            }
        }
        let mut next = before.value.clone();
        next["phase"] = json!("profile");
        next["auth"] = auth;
        if !save(&before, next)? {
            return Ok(visible(&load()?.value));
        }
    }
    let before = load()?;
    if before.value["phase"] != "profile" || before.value["pollLease"] != _lease.ticket {
        return Ok(visible(&before.value));
    }
    let auth = &before.value["auth"];
    let state = util::text(&before.value, &["state"]);
    let token = util::strip(&util::text(auth, &["accessToken", "access_token", "token"]));
    let expected = util::claims(&token)
        .map(|c| util::text(&c, &["sub", "userId", "uid"]))
        .unwrap_or_default();
    let profile = network::raw_timeout(
        "GET",
        endpoints::ORIGIN,
        &format!(
            "/v2/plugin/login/account?state={}",
            endpoints::encode(&state)
        ),
        headers::login_account(auth, true)?,
        None,
        poll_time(&before.value, 4000)?,
    );
    if let Some(result) = poll_current(&before, &_lease.ticket)? {
        return Ok(result);
    }
    let attempts = before.value["profileAttempts"].as_u64().unwrap_or(0);
    let mut profile = match profile {
        Ok(reply) if reply.code() == Some(12151) && attempts < 5 => {
            let mut next = before.value.clone();
            next["profileAttempts"] = json!(attempts + 1);
            if !save(&before, next.clone())? {
                return Ok(visible(&load()?.value));
            }
            return Ok(visible(&next));
        }
        Ok(reply) => reply.checked(&[&token, &state]).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };
    let mut list = Value::Null;
    if util::text(&profile, &["uid"]).is_empty() {
        list = network::raw_timeout(
            "GET",
            endpoints::ORIGIN,
            "/v2/plugin/accounts",
            headers::login_account(auth, false)?,
            None,
            poll_time(&before.value, 4000)?,
        )
        .and_then(|reply| reply.checked(&[&token]))
        .unwrap_or(Value::Null);
        if let Some(result) = poll_current(&before, &_lease.ticket)? {
            return Ok(result);
        }
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
                headers::login_account(auth, false)?,
                None,
                poll_time(&before.value, 4000)?,
            )
            .and_then(|reply| reply.checked(&[&token]))
            .unwrap_or(Value::Null);
            if let Some(result) = poll_current(&before, &_lease.ticket)? {
                return Ok(result);
            }
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
            let failed = json!({ "phase": "failed", "until": before.value["until"], "message": "官方已选择国际企业账号，但所属企业无法唯一确认；请重新登录或导入完整登录态" });
            if !save(&before, failed.clone())? {
                return Ok(visible(&load()?.value));
            }
            return Ok(visible(&failed));
        }
    }
    let uid = util::text(&profile, &["uid"]);
    let result = if uid.is_empty() || !expected.is_empty() && uid != expected {
        json!({ "phase": "failed", "until": before.value["until"], "message": "授权成功但国际账号身份无法确认，请重新登录" })
    } else {
        match account::parse(
            &json!({ "account": profile, "auth": auth, "edition": "intl" }),
            None,
        )
        .and_then(account::bundle)
        {
            Ok(bundle) => {
                json!({ "phase": "done", "until": before.value["until"], "account": bundle })
            }
            Err(error) => {
                json!({ "phase": "failed", "until": before.value["until"], "message": error.message })
            }
        }
    };
    if let Some(result) = poll_current(&before, &_lease.ticket)? {
        return Ok(result);
    }
    if !save(&before, result.clone())? {
        return Ok(visible(&load()?.value));
    }
    Ok(visible(&result))
}
pub fn cancel(params: &Value) -> Result<Value> {
    flow(params)?;
    for _ in 0..3 {
        let before = load()?;
        if save(
            &before,
            json!({ "phase": "cancelled", "until": sdk::now().saturating_add(TTL) }),
        )? {
            return Ok(Value::Null);
        }
    }
    Err(Error::http(409, "国际登录状态正在变化，请重试取消"))
}
