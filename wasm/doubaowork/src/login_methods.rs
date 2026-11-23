//! 登录方法与三端提交保持一致，本机快照走添加账号，不创建伪OAuth流程。

use agent2api_wasm_sdk::{json, Result, Value};

use crate::{credentials, errors, gates};

pub fn methods(locale: &str) -> Value {
    let mut value = json!({ "readiness": gates::status(), "methods": [
        { "methodIndex": 0, "type": "api", "label": "粘贴完整 Cookie 快照",
            "instructions": "粘贴包含 Cookie（含 sessionid 或 sessionid_ss）和 deviceId 的登录凭据。",
            "prompts": [
                { "key": "cookie", "type": "text", "label": "完整 Cookie 或 Cookie JSON", "secret": true,
                    "placeholder": "sessionid=…; sessionid_ss=…" },
                { "key": "deviceId", "type": "text", "label": "deviceId", "secret": true, "optional": true,
                    "placeholder": "完整 JSON 已包含时无需填写" },
                { "key": "webId", "type": "text", "label": "webId", "secret": true, "optional": true },
                { "key": "clientVersion", "type": "text", "label": "客户端版本", "optional": true },
                { "key": "name", "type": "text", "label": "备注名", "optional": true }
            ] },
        { "methodIndex": 1, "type": "api", "label": "导入文本 JSON / Cookie 快照",
            "instructions": "选择或粘贴豆包工作凭据文件（含 Cookie 与 deviceId）即可导入账号。",
            "prompts": [
                { "key": "credentialText", "type": "text", "label": "快照文本", "secret": true },
                { "key": "name", "type": "text", "label": "备注名", "optional": true }
            ] },
        { "methodIndex": 2, "type": "oauth", "label": "豆包工作网页登录", "prompts": [],
            "browserMode": "plugin", "embeddedOnly": true, "manualCallback": true, "callbackInput": "code", "validatesCallback": true,
            "instructions": "在官方页面完成登录即可添加账号；远程访问面板时可粘贴完整 Cookie JSON。" },
        agent2api_wasm_sdk::login::desktop_method(3, "豆包工作 / Doubao Work", locale)
    ] });
    translate(&mut value, locale);
    value
}

pub fn start(params: &Value) -> Result<Value> {
    let index = params["payload"]["methodIndex"].as_u64();
    if index.is_some_and(|index| index != 2) || params["method"] == "desktop" {
        return Err(errors::invalid(
            "本机或凭据导入请使用添加账号入口，不发起网页登录",
        ));
    }
    agent2api_wasm_sdk::cookie_login::start(
        "doubaowork",
        "https://www.doubao.com",
        &["doubao.com", "feishu.cn", "larksuite.com"],
    )
}
pub fn poll(params: &Value) -> Result<Value> {
    let flow = params["flow"]
        .as_str()
        .filter(|flow| flow.starts_with("cookie_login_") && flow.len() <= 128)
        .ok_or_else(|| errors::invalid("登录流程缺失或无效"))?;
    let value = agent2api_wasm_sdk::session_get(flow)?;
    if value["provider"] != "doubaowork" || value["state"] != "waiting" {
        return Err(errors::invalid("登录已经过期或取消"));
    }
    Ok(json!({"state":"pending"}))
}

fn translate(value: &mut Value, locale: &str) {
    let index = match locale {
        "zh-Hant" => 0,
        "en" => 1,
        "ja" => 2,
        "ko" => 3,
        "pt-BR" => 4,
        _ => return,
    };
    match value {
        Value::String(text) => {
            if let Some((_, translated)) = LOGIN_TEXT
                .iter()
                .find(|(original, _)| *original == text.as_str())
            {
                *text = translated[index].to_string();
            }
        }
        Value::Array(rows) => rows.iter_mut().for_each(|row| translate(row, locale)),
        Value::Object(fields) => fields
            .values_mut()
            .for_each(|field| translate(field, locale)),
        _ => {}
    }
}

const LOGIN_TEXT: &[(&str, [&str; 5])] = &[
    ("粘贴完整 Cookie 快照", ["貼上完整 Cookie 快照", "Paste a complete cookie snapshot", "Cookieスナップショットを貼り付け", "전체 Cookie 스냅샷 붙여넣기", "Colar snapshot completo de cookies"]),
    ("粘贴包含 Cookie（含 sessionid 或 sessionid_ss）和 deviceId 的登录凭据。", [
        "貼上包含 Cookie（含 sessionid 或 sessionid_ss）與 deviceId 的登入憑據。",
        "Paste sign-in credentials containing cookies (sessionid or sessionid_ss) and deviceId.",
        "Cookie（sessionid または sessionid_ss）と deviceId を含む認証情報を貼り付けてください。",
        "Cookie(sessionid 또는 sessionid_ss)와 deviceId가 포함된 로그인 인증 정보를 붙여넣으세요.",
        "Cole as credenciais de login contendo cookies (sessionid ou sessionid_ss) e deviceId.",
    ]),
    ("完整 Cookie 或 Cookie JSON", ["完整 Cookie 或 Cookie JSON", "Complete cookie header or JSON", "完全なCookieまたはCookie JSON", "전체 Cookie 또는 Cookie JSON", "Cookie completo ou JSON de cookies"]),
    ("完整 JSON 已包含时无需填写", ["完整 JSON 已包含時無需填寫", "Optional if already included in the JSON", "JSONに含まれていれば入力不要", "JSON에 포함되어 있으면 입력하지 않아도 됩니다", "Opcional se já estiver no JSON"]),
    ("客户端版本", ["用戶端版本", "Client version", "クライアントのバージョン", "클라이언트 버전", "Versão do cliente"]),
    ("备注名", ["備註名稱", "Account label", "表示名", "계정 표시 이름", "Nome da conta"]),
    ("导入文本 JSON / Cookie 快照", ["匯入文字 JSON / Cookie 快照", "Import JSON / cookie snapshot", "JSON / Cookieスナップショットを取り込み", "JSON / Cookie 스냅샷 가져오기", "Importar JSON / snapshot de cookies"]),
    ("选择或粘贴豆包工作凭据文件（含 Cookie 与 deviceId）即可导入账号。", [
        "選擇或貼上豆包工作憑據檔案（含 Cookie 與 deviceId）即可匯入帳號。",
        "Select or paste a Doubao Work credential file (with cookies and deviceId) to import the account.",
        "Doubao Work の認証ファイル（Cookie と deviceId を含む）を選択または貼り付けてアカウントを取り込めます。",
        "Doubao Work 인증 파일(Cookie 및 deviceId 포함)을 선택하거나 붙여넣어 계정을 가져올 수 있습니다.",
        "Selecione ou cole um arquivo de credenciais do Doubao Work (com cookies e deviceId) para importar a conta.",
    ]),
    ("快照文本", ["快照文字", "Snapshot text", "スナップショットのテキスト", "스냅샷 텍스트", "Texto do snapshot"]),
    ("豆包工作网页登录", ["豆包工作網頁登入", "Doubao Work browser sign-in", "Doubao Workウェブログイン", "Doubao Work 웹 로그인", "Login web do Doubao Work"]),
    ("在官方页面完成登录即可添加账号；远程访问面板时可粘贴完整 Cookie JSON。", [
        "在官方頁面完成登入即可新增帳號；遠端存取面板時可貼上完整 Cookie JSON。",
        "Sign in on the official page to add the account; when accessing the panel remotely, paste the complete cookie JSON.",
        "公式ページでログインするとアカウントを追加できます。リモート画面では完全な Cookie JSON を貼り付けてください。",
        "공식 페이지에서 로그인하면 계정이 추가됩니다. 원격 패널에서는 전체 Cookie JSON을 붙여넣으세요.",
        "Conclua o login na página oficial para adicionar a conta; no acesso remoto ao painel, cole o JSON completo de cookies.",
    ]),
];

pub fn input(params: &Value) -> Result<(Value, &'static str, Option<String>)> {
    let source = &params["source"];
    match source["kind"].as_str() {
        Some("desktop") => Ok((
            agent2api_wasm_sdk::host("desktop.credentials.read", json!({}))?,
            "desktop",
            None,
        )),
        Some("file_base64") => Err(errors::invalid(
            "Cookie 数据库请通过已授权的本机导入入口读取；文本导入不接受二进制数据库",
        )),
        Some("file_text") => {
            let text = source["content"]
                .as_str()
                .ok_or_else(|| errors::invalid("豆包工作快照内容必须是文本"))?;
            Ok((credentials::payload(&json!(text))?, "file", None))
        }
        Some("manual") => {
            let payload = &source["payload"];
            if payload.get("methodIndex").is_none() {
                return Ok((credentials::payload(payload)?, "manual", None));
            }
            let index = payload["methodIndex"]
                .as_u64()
                .filter(|index| *index <= 1)
                .ok_or_else(|| errors::invalid("请选择豆包工作手工或文本快照导入"))?;
            let fields = payload["inputs"]
                .as_object()
                .ok_or_else(|| errors::invalid("豆包工作登录字段必须是对象"))?;
            let key = if index == 0 {
                "cookie"
            } else {
                "credentialText"
            };
            let allowed = if index == 0 {
                vec![key, "name", "deviceId", "webId", "clientVersion"]
            } else {
                vec![key, "name"]
            };
            if fields
                .iter()
                .any(|(name, value)| !allowed.contains(&name.as_str()) || !value.is_string())
            {
                return Err(errors::invalid("豆包工作导入包含未申报字段或非文本输入"));
            }
            let text = fields
                .get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| errors::invalid("请提供豆包工作完整 Cookie 快照"))?;
            let name = fields
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string);
            let mut input = credentials::payload(&json!(text))?;
            if index == 0 {
                if !input.is_object() {
                    input = json!({ "cookies": input });
                }
                for key in ["deviceId", "webId", "clientVersion"] {
                    if let Some(value) = fields
                        .get(key)
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                    {
                        if input
                            .get(key)
                            .and_then(Value::as_str)
                            .is_some_and(|existing| !existing.is_empty() && existing != value)
                        {
                            return Err(errors::invalid(
                                "豆包工作快照与表单中的客户端信息不一致，请保留原登录上下文",
                            ));
                        }
                        input[key] = json!(value);
                    }
                }
            }
            Ok((input, "manual", name))
        }
        _ => Err(errors::invalid("请选择豆包工作手工或文本快照导入")),
    }
}
