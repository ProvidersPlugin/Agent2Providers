//! 国际官方网页登录与手工凭据入口；只读取本次官方桌面窗口，不借用国内登录态。

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
        "粘贴 MonkeyCode 国际站的 Session 或 Cookie 即可添加账号。",
        "Paste a Session or Cookie from MonkeyCode International to add the account.",
        "貼上 MonkeyCode 國際站的 Session 或 Cookie 即可新增帳號。",
        "MonkeyCode 国際サイトの Session または Cookie を貼り付けてアカウントを追加できます。",
        "MonkeyCode 국제 사이트의 Session 또는 Cookie를 붙여넣어 계정을 추가할 수 있습니다.",
        "Cole uma Session ou Cookie do MonkeyCode International para adicionar a conta.",
    ])
}

fn file_hint(locale: &str) -> &'static str {
    text(locale, [
        "选择或粘贴 MonkeyCode 国际版凭据文件即可导入账号。",
        "Select or paste a MonkeyCode International credential file to import the account.",
        "選擇或貼上 MonkeyCode 國際版憑據檔案即可匯入帳號。",
        "MonkeyCode 国際版の認証ファイルを選択または貼り付けてアカウントを取り込めます。",
        "MonkeyCode 국제판 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있습니다.",
        "Selecione ou cole um arquivo de credenciais do MonkeyCode International para importar a conta.",
    ])
}

fn web_hint(locale: &str) -> &'static str {
    text(locale, [
        "在桌面窗口完成国际站官方登录即可自动添加账号；网页和手机面板可导入国际版 Session 或凭据文件。",
        "Sign in on the international site in the desktop window to add the account automatically; web and mobile panels can import an international Session or credential file.",
        "在桌面視窗完成國際站官方登入即可自動新增帳號；網頁及手機面板可匯入國際版 Session 或憑據檔案。",
        "デスクトップの画面で国際サイトの公式ログインを完了すると自動でアカウントを追加します。Web・モバイルでは国際版 Session または認証ファイルを取り込めます。",
        "데스크톱 창에서 국제 사이트 공식 로그인을 완료하면 계정이 자동 추가됩니다. 웹과 모바일에서는 국제판 Session 또는 인증 파일을 가져올 수 있습니다.",
        "Conclua o login oficial internacional na janela desktop para adicionar a conta automaticamente; os painéis web e móvel podem importar Session ou arquivo de credenciais internacional.",
    ])
}

pub fn methods(locale: &str) -> Value {
    let manual = text(
        locale,
        [
            "粘贴国际 Session／Cookie",
            "Paste international Session / Cookie",
            "貼上國際 Session／Cookie",
            "国際版 Session／Cookie を貼り付け",
            "국제 Session / Cookie 붙여넣기",
            "Colar Session / Cookie internacional",
        ],
    );
    let web = text(
        locale,
        [
            "MonkeyCode 国际版网页登录",
            "Sign in to MonkeyCode International",
            "MonkeyCode 國際版網頁登入",
            "MonkeyCode 国際版にログイン",
            "MonkeyCode 국제 로그인",
            "Entrar no MonkeyCode Internacional",
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
            "导入完整国际 JSON",
            "Import complete international JSON",
            "匯入完整國際 JSON",
            "国際版の完全な JSON を取り込む",
            "전체 국제 JSON 가져오기",
            "Importar JSON internacional completo",
        ],
    );
    json!({"readiness":gates::status(),"methods":[
        {"methodIndex":0,"type":"api","label":manual,"instructions":manual_hint(locale),"prompts":[
            {"key":"session","type":"text","label":"Session / Cookie / JSON","secret":true,"placeholder":"monkeycode_ai_session=…"},
            {"key":"name","type":"text","label":name,"optional":true},
            {"key":"imageId","type":"text","label":image,"optional":true}]},
        {"methodIndex":1,"type":"oauth","label":web,"instructions":web_hint(locale),"prompts":[],
            "browserMode":"choice","embeddedOnly":true,"desktopOnly":true,"manualCallback":false,"validatesCallback":true},
        {"methodIndex":2,"type":"api","label":json_label,"instructions":file_hint(locale),"prompts":[
            {"key":"credentialJson","type":"text","label":json_label,"secret":true},
            {"key":"name","type":"text","label":name,"optional":true}]}
    ]})
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
        &["monkeycode-ai.net", "github.com", "accounts.google.com"],
    )?;
    let flow = started["flow"]
        .as_str()
        .ok_or_else(Error::unsupported)?
        .to_string();
    sdk::session_put(
        &flow,
        json!({"provider":endpoints::ID,"state":"waiting","locale":locale}),
        300_000,
    )?;
    started["instructions"] = json!(web_hint(locale));
    Ok(started)
}

pub fn callback(params: &Value) -> Result<Value> {
    sdk::cookie_login::callback(endpoints::ID, params, account::normalize).and_then(|mut result| {
        if result["account"]["secret"].is_object() {
            result["account"]["secret"]["source"] = json!("web");
            result["account"]["public"] = account::view(&result["account"]["secret"])?;
        }
        Ok(result)
    })
}

pub fn poll(params: &Value) -> Result<Value> {
    let flow = params["flow"]
        .as_str()
        .filter(|flow| flow.starts_with("cookie_login_") && flow.len() <= 128)
        .ok_or_else(|| Error::http(400, "MonkeyCode 国际登录流程无效"))?;
    let pending = sdk::session_get(flow)?;
    let locale = pending["locale"].as_str().unwrap_or("zh-CN");
    if pending["provider"] != endpoints::ID || pending["state"] != "waiting" {
        return Ok(json!({"state":"failed","message":text(locale,[
            "国际登录已取消或过期，请重新发起", "International sign-in was canceled or expired; start again",
            "國際登入已取消或過期，請重新發起", "国際版のログインがキャンセルされたか期限切れです。再開してください",
            "국제 로그인이 취소되었거나 만료되었습니다. 다시 시작하세요", "O login internacional foi cancelado ou expirou; inicie novamente"
        ])}));
    }
    Ok(json!({"state":"pending","message":web_hint(locale)}))
}
