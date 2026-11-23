//! 官方登录由本次桌面独立窗口完成；Cookie 含 HttpOnly，不是 OAuth 授权码。
//! 网页及手机保留凭据导入，本机读取只经用户明确选择的固定客户端入口。

use crate::{account, endpoints, gates};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub fn methods(locale: &str) -> Value {
    let (web, web_hint, cookie, cookie_hint, file, file_hint, note, credentials) = match locale {
        "en" => ("Sign in to KukuAI", "Complete SMS or QR sign-in in the desktop window to add the account automatically; web and mobile panels can paste cookies.", "Paste cookies", "Paste the Cookie header (including BDUSS) from a signed-in KukuAI account.", "Import credential JSON", "Select or paste a KukuAI credential file to import the account.", "Note", "Credential JSON"),
        "ja" => ("KukuAI にログイン", "デスクトップの画面で SMS または QR ログインを完了すると自動でアカウントを追加します。Web・モバイルでは Cookie を貼り付けてください。", "Cookie を貼り付け", "ログイン済みの KukuAI アカウントの Cookie（BDUSS を含む）を貼り付けてください。", "認証 JSON を取り込む", "KukuAI の認証ファイルを選択または貼り付けてアカウントを取り込めます。", "メモ", "認証 JSON"),
        "ko" => ("KukuAI 로그인", "데스크톱 창에서 문자 코드 또는 QR 로그인을 완료하면 계정이 자동 추가됩니다. 웹과 모바일에서는 Cookie를 붙여넣으세요.", "쿠키 붙여넣기", "로그인된 KukuAI 계정의 Cookie(BDUSS 포함)를 붙여넣으세요.", "인증 JSON 가져오기", "KukuAI 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있습니다.", "메모", "인증 JSON"),
        "pt-BR" => ("Entrar no KukuAI", "Conclua o login por SMS ou QR na janela do aplicativo desktop para adicionar a conta automaticamente; no painel web ou celular, cole os cookies.", "Colar cookies", "Cole o Cookie (incluindo BDUSS) de uma conta KukuAI conectada.", "Importar JSON de credenciais", "Selecione ou cole um arquivo de credenciais do KukuAI para importar a conta.", "Nota", "JSON de credenciais"),
        "zh-Hant" => ("庫庫網頁登入", "在桌面視窗完成手機驗證碼或掃碼登入即可自動新增帳號；網頁及手機面板可貼上 Cookie。", "貼上 Cookie", "貼上已登入庫庫帳號的 Cookie（需包含 BDUSS）即可新增帳號。", "匯入憑據 JSON", "選擇或貼上庫庫憑據檔案即可匯入帳號。", "備註名", "憑據 JSON"),
        _ => ("库库网页登录", "在桌面窗口完成手机验证码或扫码登录即可自动添加账号；网页和手机面板可粘贴 Cookie。", "粘贴 Cookie", "粘贴已登录库库账号的 Cookie（需包含 BDUSS）即可添加账号。", "导入凭据 JSON", "选择或粘贴库库凭据文件即可导入账号。", "备注名", "凭据 JSON"),
    };
    let desktop = sdk::login::desktop_method(3, "库库 / KukuAI", locale);
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": web, "instructions": web_hint,
            "prompts": [], "localOnly": false, "manualCallback": false, "callbackInput": "url",
            "browserMode": "plugin", "embeddedOnly": true, "desktopOnly": true, "validatesCallback": true },
        { "methodIndex": 1, "type": "api", "label": cookie, "instructions": cookie_hint,
            "prompts": [{ "key": "cookie", "type": "text", "label": "Cookie", "secret": true },
                { "key": "name", "type": "text", "label": note, "optional": true }] },
        { "methodIndex": 2, "type": "api", "label": file, "instructions": file_hint,
            "prompts": [{ "key": "credentialJson", "type": "text", "label": credentials, "secret": true },
                { "key": "name", "type": "text", "label": note, "optional": true }] },
        desktop
    ] })
}

pub fn start(params: &Value) -> Result<Value> {
    let payload = &params["payload"];
    if payload
        .get("methodIndex")
        .is_some_and(|index| index.as_u64() != Some(0))
    {
        return Err(gates::desktop_error());
    }
    if payload["mode"] == "external" {
        return Err(Error::http(
            400,
            "库库网页登录需要桌面内嵌窗口；系统浏览器和手机请用粘贴 Cookie",
        ));
    }
    if payload
        .get("inputs")
        .is_some_and(|value| value.as_object().is_none_or(|fields| !fields.is_empty()))
    {
        return Err(Error::http(400, "库库网页登录不接受额外凭据或自动填充字段"));
    }
    let mut value = sdk::cookie_login::start(
        endpoints::ID,
        endpoints::BASE,
        &[
            "kuku.baidu.com",
            "passport.baidu.com",
            "wappass.baidu.com",
            "baidu.com",
        ],
    )?;
    // 由 auth.methods 的本次 locale 说明展示；不覆盖为 SDK 缺省中文。
    value["instructions"] = json!("");
    value["login_ui"] = json!({"browserMode":"plugin","manualCallback":false,"callbackInput":"url","captureCallback":false});
    Ok(value)
}

pub fn callback(params: &Value) -> Result<Value> {
    let mut value = sdk::cookie_login::callback(endpoints::ID, params, account::normalize)?;
    value["account"]["secret"]["source"] = json!("browser");
    value["account"]["public"] = account::view(&value["account"]["secret"])?;
    Ok(value)
}

pub fn poll(params: &Value) -> Result<Value> {
    let flow = params["flow"]
        .as_str()
        .filter(|flow| flow.starts_with("cookie_login_") && flow.len() <= 128)
        .ok_or_else(|| Error::http(400, "库库登录流程无效"))?;
    let pending = sdk::session_get(flow)?;
    if pending["provider"] != endpoints::ID || pending["state"] != "waiting" {
        return Ok(json!({"state":"failed","message":"库库登录已取消或过期，请重新发起"}));
    }
    Ok(json!({"state":"pending"}))
}

pub fn cancel(params: &Value) -> Result<Value> {
    sdk::cookie_login::cancel(params)
}
