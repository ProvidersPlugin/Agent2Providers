//! 登录只捕获本次官方桌面窗口；远程面板保留 Cookie / JSON，原索引不变。
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

use crate::{account, endpoints};

fn captions(locale: &str) -> [&'static str; 8] {
    match locale {
        "en" => ["Paste DuMate cookies", "Paste the Cookie header from a signed-in DuMate account.", "Import one credential JSON", "Select or paste a DuMate credential file to import the account.", "Sign in to DuMate", "Complete QR or SMS sign-in in the desktop window to add the account automatically; web and mobile panels can paste cookies.", "Note", "Credential JSON"],
        "ja" => ["DuMate Cookie を貼り付け", "ログイン済みの DuMate アカウントの Cookie を貼り付けてください。", "単一アカウントの認証 JSON", "DuMate の認証ファイルを選択または貼り付けてアカウントを取り込めます。", "DuMate にログイン", "デスクトップの画面で QR または SMS ログインを完了するとアカウントを追加できます。Web・モバイルでは Cookie を貼り付けてください。", "メモ", "認証 JSON"],
        "ko" => ["DuMate 쿠키 붙여넣기", "로그인된 DuMate 계정의 Cookie를 붙여넣으세요.", "계정 하나의 인증 JSON", "DuMate 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있습니다.", "DuMate 로그인", "데스크톱 창에서 QR 또는 문자 코드 로그인을 완료하면 계정이 추가됩니다. 웹·모바일에서는 Cookie를 붙여넣으세요.", "메모", "인증 JSON"],
        "pt-BR" => ["Colar cookies do DuMate", "Cole o Cookie de uma conta DuMate conectada.", "Importar JSON de uma conta", "Selecione ou cole um arquivo de credenciais do DuMate para importar a conta.", "Entrar no DuMate", "Conclua o login por QR ou SMS na janela do aplicativo desktop para adicionar a conta; no painel web/celular, cole os cookies.", "Nota", "JSON de credenciais"],
        "zh-Hant" => ["貼上百度搭子 Cookie", "貼上已登入百度搭子帳號的 Cookie 即可新增帳號。", "匯入單帳號憑據 JSON", "選擇或貼上百度搭子憑據檔案即可匯入帳號。", "百度搭子網頁登入", "在桌面視窗完成掃碼或手機驗證碼登入即可新增帳號；網頁及手機面板可貼上 Cookie。", "備註名", "憑據 JSON"],
        _ => ["粘贴百度搭子 Cookie", "粘贴已登录百度搭子账号的 Cookie 即可添加账号。", "导入单账号凭据 JSON", "选择或粘贴百度搭子凭据文件即可导入账号。", "百度搭子网页登录", "在桌面窗口完成扫码或手机验证码登录即可添加账号；网页和手机面板可粘贴 Cookie。", "备注名", "凭据 JSON"],
    }
}

pub fn methods(locale: &str) -> Value {
    let [cookie, cookie_hint, file, file_hint, browser, browser_hint, note, credential] =
        captions(locale);
    let desktop = sdk::login::desktop_method(3, "百度搭子 DuMate", locale);
    json!({"loginUrl":endpoints::LOGIN,"methods":[
        {"methodIndex":0,"type":"api","label":cookie,"instructions":cookie_hint,"prompts":[
            {"key":"cookie","type":"text","label":"Cookie","secret":true,"placeholder":"BDUSS=…; STOKEN=…; bce-long-term-sessionid=…"},
            {"key":"name","type":"text","label":note,"optional":true}]},
        {"methodIndex":1,"type":"api","label":file,"instructions":file_hint,"prompts":[
            {"key":"credentialJson","type":"text","label":credential,"secret":true},
            {"key":"name","type":"text","label":note,"optional":true}]},
        {"methodIndex":2,"type":"oauth","label":browser,"instructions":browser_hint,"prompts":[],
            "localOnly":false,"manualCallback":false,"callbackInput":"url","browserMode":"plugin",
            "embeddedOnly":true,"desktopOnly":true,"validatesCallback":true}, desktop]})
}

pub fn start(params: &Value) -> Result<Value> {
    let payload = &params["payload"];
    if payload
        .get("methodIndex")
        .is_some_and(|index| index.as_u64() != Some(2))
        || payload["mode"] == "external"
        || payload
            .get("inputs")
            .is_some_and(|inputs| inputs.as_object().is_none_or(|fields| !fields.is_empty()))
    {
        return Err(Error::http(
            400,
            "百度搭子网页登录仅支持桌面官方窗口，请改用 Cookie / JSON",
        ));
    }
    let mut started = sdk::cookie_login::start(
        endpoints::PROVIDER,
        endpoints::CONSOLE,
        &[
            "login.bce.baidu.com",
            "console.bce.baidu.com",
            "passport.baidu.com",
            "wappass.baidu.com",
            "www.dumate.cn",
        ],
    )?;
    started["url"] = json!(endpoints::LOGIN);
    started["instructions"] = json!(captions(payload["locale"].as_str().unwrap_or("zh-CN"))[5]);
    started["login_ui"] = json!({"browserMode":"plugin","manualCallback":false,"callbackInput":"url","captureCallback":false});
    Ok(started)
}

pub fn callback(params: &Value) -> Result<Value> {
    sdk::cookie_login::callback(endpoints::PROVIDER, params, account::normalize)
}

pub fn poll(params: &Value) -> Result<Value> {
    let flow = params["flow"]
        .as_str()
        .filter(|flow| flow.starts_with("cookie_login_") && flow.len() <= 128)
        .ok_or_else(|| Error::http(400, "百度搭子登录流程无效"))?;
    let pending = sdk::session_get(flow)?;
    Ok(
        if pending["provider"] == endpoints::PROVIDER && pending["state"] == "waiting" {
            json!({"state":"pending"})
        } else {
            json!({"state":"failed","message":"百度搭子登录已取消或过期，请重新发起"})
        },
    )
}

pub fn cancel(params: &Value) -> Result<Value> {
    sdk::cookie_login::cancel(params)
}
