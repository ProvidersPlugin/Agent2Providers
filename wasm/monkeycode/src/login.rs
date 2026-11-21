//! 国内官方登录方式。网页登录只由本家桌面窗口捕获 Cookie，远程端保留凭据导入。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

use crate::{account, endpoints, gates};

fn text(locale: &str, items: [&'static str; 6]) -> &'static str {
    items[match locale {
        "en" => 1,
        "zh-Hant" => 2,
        "ja" => 3,
        "ko" => 4,
        "pt-BR" => 5,
        _ => 0,
    }]
}

fn manual_hint(locale: &str) -> &'static str {
    text(locale, [
        "粘贴 MonkeyCode 国内站的 Session 或 Cookie 即可添加账号。",
        "Paste a Session or Cookie from MonkeyCode China to add the account.",
        "貼上 MonkeyCode 國內站的 Session 或 Cookie 即可新增帳號。",
        "MonkeyCode 中国サイトの Session または Cookie を貼り付けてアカウントを追加できます。",
        "MonkeyCode 중국 사이트의 Session 또는 Cookie를 붙여넣어 계정을 추가할 수 있습니다.",
        "Cole uma Session ou Cookie do MonkeyCode China para adicionar a conta.",
    ])
}

fn file_hint(locale: &str) -> &'static str {
    text(locale, [
        "选择或粘贴 MonkeyCode 国内版凭据文件即可导入账号。",
        "Select or paste a MonkeyCode China credential file to import the account.",
        "選擇或貼上 MonkeyCode 國內版憑據檔案即可匯入帳號。",
        "MonkeyCode 中国版の認証ファイルを選択または貼り付けてアカウントを取り込めます。",
        "MonkeyCode 중국판 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있습니다.",
        "Selecione ou cole um arquivo de credenciais do MonkeyCode China para importar a conta.",
    ])
}

fn web_hint(locale: &str) -> &'static str {
    text(locale, [
        "在桌面窗口完成国内站官方登录即可自动添加账号；网页和手机面板可导入 Session 或凭据文件。",
        "Sign in on the Chinese site in the desktop window to add the account automatically; web and mobile panels can import a Session or credential file.",
        "在桌面視窗完成國內站官方登入即可自動新增帳號；網頁及手機面板可匯入 Session 或憑據檔案。",
        "デスクトップの画面で中国サイトの公式ログインを完了すると自動でアカウントを追加します。Web・モバイルでは Session または認証ファイルを取り込めます。",
        "데스크톱 창에서 중국 사이트 공식 로그인을 완료하면 계정이 자동 추가됩니다. 웹과 모바일에서는 Session 또는 인증 파일을 가져올 수 있습니다.",
        "Conclua o login oficial no site chinês na janela desktop para adicionar a conta automaticamente; os painéis web e móvel podem importar Session ou arquivo de credenciais.",
    ])
}

pub fn methods(locale: &str) -> Value {
    let manual = text(
        locale,
        [
            "粘贴 Session／Cookie",
            "Paste Session / Cookie",
            "貼上 Session／Cookie",
            "Session／Cookie を貼り付け",
            "Session / Cookie 붙여넣기",
            "Colar Session / Cookie",
        ],
    );
    let web = text(
        locale,
        [
            "MonkeyCode 网页登录",
            "Sign in to MonkeyCode",
            "MonkeyCode 網頁登入",
            "MonkeyCode にログイン",
            "MonkeyCode 로그인",
            "Entrar no MonkeyCode",
        ],
    );
    let name = text(locale, ["备注名", "Note", "備註", "メモ", "메모", "Nota"]);
    let image = text(
        locale,
        [
            "执行环境镜像编号（可选）",
            "VM image ID (optional)",
            "鏡像編號（選填）",
            "イメージ ID（任意）",
            "이미지 ID (선택)",
            "ID da imagem (opcional)",
        ],
    );
    let json_label = text(
        locale,
        [
            "导入完整 JSON",
            "Import complete JSON",
            "匯入完整 JSON",
            "完全な JSON を取り込む",
            "전체 JSON 가져오기",
            "Importar JSON completo",
        ],
    );
    json!({ "readiness": gates::status(), "methods": [
        { "methodIndex": 0, "type": "api", "label": manual, "instructions": manual_hint(locale), "prompts": [
            { "key": "session", "type": "text", "label": "Session / Cookie", "secret": true, "placeholder": "monkeycode_ai_session=…" },
            { "key": "name", "type": "text", "label": name, "optional": true },
            { "key": "imageId", "type": "text", "label": image, "optional": true }
        ] },
        { "methodIndex": 1, "type": "oauth", "label": web, "instructions": web_hint(locale), "prompts": [],
            "browserMode": "choice", "embeddedOnly": true, "desktopOnly": true, "manualCallback": false,
            "validatesCallback": true },
        { "methodIndex": 2, "type": "api", "label": json_label, "instructions": file_hint(locale), "prompts": [
            { "key": "credentialJson", "type": "text", "label": json_label, "secret": true },
            { "key": "name", "type": "text", "label": name, "optional": true }
        ] }
    ] })
}

pub fn start(params: &Value) -> Result<Value> {
    let options = &params["payload"];
    if options
        .get("methodIndex")
        .is_some_and(|index| index.as_u64() != Some(1))
        || options.get("mode").is_some_and(|mode| mode != "embedded")
        || options.get("inputs").is_some_and(|inputs| {
            !inputs.is_object() || inputs.as_object().is_some_and(|fields| !fields.is_empty())
        })
    {
        return Err(gates::web_login());
    }
    let locale = options["locale"].as_str().unwrap_or("zh-CN");
    let mut started = sdk::cookie_login::start(
        endpoints::ID,
        endpoints::ORIGIN,
        &["monkeycode-ai.com", "baizhi.cloud"],
    )?;
    let flow = started["flow"]
        .as_str()
        .ok_or_else(Error::unsupported)?
        .to_string();
    sdk::session_put(
        &flow,
        json!({ "provider": endpoints::ID, "state": "waiting", "locale": locale }),
        300_000,
    )?;
    started["instructions"] = json!(web_hint(locale));
    Ok(started)
}

pub fn callback(params: &Value) -> Result<Value> {
    sdk::cookie_login::callback(endpoints::ID, params, account::normalize).map(|mut result| {
        if result["account"]["secret"].is_object() {
            result["account"]["secret"]["source"] = json!("web");
            result["account"]["public"] = account::view(&result["account"]["secret"]);
        }
        result
    })
}

pub fn poll(params: &Value) -> Result<Value> {
    let flow = params["flow"]
        .as_str()
        .filter(|flow| flow.starts_with("cookie_login_") && flow.len() <= 128)
        .ok_or_else(|| Error::http(400, "MonkeyCode 登录流程无效"))?;
    let pending = sdk::session_get(flow)?;
    if pending["provider"] != endpoints::ID || pending["state"] != "waiting" {
        return Ok(
            json!({ "state": "failed", "message": "MonkeyCode 登录已取消或过期，请重新发起" }),
        );
    }
    Ok(
        json!({ "state": "pending", "message": web_hint(pending["locale"].as_str().unwrap_or("zh-CN")) }),
    )
}
