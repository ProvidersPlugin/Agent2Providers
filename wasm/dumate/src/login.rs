//! 登录只捕获本次官方桌面窗口；远程面板保留 Cookie / JSON，原索引不变。
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

use crate::{account, endpoints};

fn captions(locale: &str) -> [&'static str; 8] {
    match locale {
        "en" => ["Paste DuMate cookies", "Paste the complete Cookie header from one signed-in DuMate account. Model access tokens can be renewed, but an expired Baidu web session requires signing in again.", "Import one credential JSON", "Accepts a cookie-editor export or one account's JSON containing cookie/cookies. An entire account pool or binary database is not imported.", "Sign in to DuMate", "Complete the official QR or phone-code sign-in in the desktop app's private window. Only that window's cookies are read and the account is verified before saving. Web/mobile panels can paste cookies instead.", "Note", "Credential JSON"],
        "ja" => ["DuMate Cookie を貼り付け", "ログイン済みの単一 DuMate アカウントの完全な Cookie ヘッダーを貼り付けます。モデル用トークンは更新できますが、百度の Web セッションが切れた場合は再ログインが必要です。", "単一アカウントの認証 JSON", "Cookie エディターの出力または cookie/cookies を含む単一アカウント JSON に対応します。アカウントプールやバイナリ DB は取り込みません。", "DuMate にログイン", "デスクトップアプリの専用公式画面で QR または SMS ログインを完了します。その画面の Cookie のみ読み、保存前にアカウントを確認します。Web・モバイルでは Cookie を貼り付けてください。", "メモ", "認証 JSON"],
        "ko" => ["DuMate 쿠키 붙여넣기", "로그인된 DuMate 계정 하나의 전체 Cookie 헤더를 붙여 넣으세요. 모델 토큰은 갱신할 수 있지만 Baidu 웹 세션이 만료되면 다시 로그인해야 합니다.", "계정 하나의 인증 JSON", "쿠키 편집기 내보내기나 cookie/cookies를 포함한 단일 계정 JSON을 지원합니다. 계정 풀이나 이진 DB는 가져오지 않습니다.", "DuMate 로그인", "데스크톱 앱의 공식 전용 창에서 QR 또는 문자 코드 로그인을 완료하세요. 이 창의 쿠키만 읽고 저장 전 계정을 확인합니다. 웹·모바일에서는 쿠키를 붙여 넣으세요.", "메모", "인증 JSON"],
        "pt-BR" => ["Colar cookies do DuMate", "Cole o cabeçalho Cookie completo de uma única conta DuMate conectada. Tokens do modelo podem ser renovados, mas uma sessão web Baidu expirada exige novo login.", "Importar JSON de uma conta", "Aceita exportação do editor de cookies ou JSON de uma conta com cookie/cookies. Não importa o conjunto de contas nem bancos binários.", "Entrar no DuMate", "Conclua o login oficial por QR ou SMS na janela privada do aplicativo desktop. Só os cookies dessa janela são lidos e a conta é verificada antes de salvar. No painel web/celular, cole cookies.", "Nota", "JSON de credenciais"],
        "zh-Hant" => ["貼上百度搭子 Cookie", "貼上單一百度搭子帳號的完整 Cookie。模型令牌可自動換發，百度網頁登入態過期後仍需重新登入。", "匯入單帳號憑據 JSON", "接受 Cookie 編輯器匯出，或包含 cookie/cookies 的單帳號 JSON；不匯入整個帳號池或二進位資料庫。", "百度搭子網頁登入", "請在桌面應用的獨立官方視窗完成掃碼或手機驗證碼登入；只讀取本次視窗的 Cookie，儲存前核對帳號。網頁及手機面板可貼上 Cookie。", "備註名", "憑據 JSON"],
        _ => ["粘贴百度搭子 Cookie", "粘贴单个百度搭子账号的完整 Cookie。模型令牌可自动换发，百度网页登录态过期后仍需重新登录。", "导入单账号凭据 JSON", "接受 Cookie 编辑器导出，或包含 cookie/cookies 的单账号 JSON；不导入整个账号池或二进制数据库。", "百度搭子网页登录", "请在桌面应用的独立官方窗口完成扫码或手机验证码登录；只读取本次窗口的 Cookie，保存前核对账号。网页和手机面板可粘贴 Cookie。", "备注名", "凭据 JSON"],
    }
}

pub fn methods(locale: &str) -> Value {
    let [cookie, cookie_hint, file, file_hint, browser, browser_hint, note, credential] =
        captions(locale);
    let mut desktop = sdk::login::desktop_method(3, "百度搭子 DuMate", locale);
    let snapshot = match locale {
        "en" => "Saves a one-time snapshot of the active personal account, not a live client link; enterprise/custom servers are not imported.",
        "ja" => "現在の個人アカウントのスナップショットを保存します。継続連携や企業・独自サーバーの取り込みは行いません。",
        "ko" => "현재 개인 계정의 일회성 스냅샷을 저장합니다. 실시간 연결이나 기업·사용자 지정 서버 가져오기는 지원하지 않습니다.",
        "pt-BR" => "Salva uma cópia única da conta pessoal ativa, não um vínculo em tempo real; servidores empresariais/personalizados não são importados.",
        "zh-Hant" => "只保存目前個人帳號的一次性快照，不持續連結官方用戶端；不匯入企業或自訂伺服器。",
        _ => "只保存当前个人账号的一次性快照，不持续关联官方客户端；不导入企业或自定义服务器。",
    };
    desktop["instructions"] = json!(format!(
        "{}\n{snapshot}",
        desktop["instructions"].as_str().unwrap_or("")
    ));
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
