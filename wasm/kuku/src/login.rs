//! 官方登录由本次桌面独立窗口完成；Cookie 含 HttpOnly，不是 OAuth 授权码。
//! 网页及手机保留凭据导入，本机读取只经用户明确选择的固定客户端入口。

use crate::{account, endpoints, gates};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub fn methods(locale: &str) -> Value {
    let (web, web_hint, cookie, cookie_hint, file, file_hint, note, credentials) = match locale {
        "en" => ("Sign in to KukuAI", "Complete phone-code or QR sign-in in the desktop app's official window. This flow alone reads its own cookies and adds the account automatically. Web and mobile panels can paste cookies instead.", "Paste cookies", "Paste the complete Cookie header or a cookie-editor JSON export from one KukuAI session. BDUSS is required; STOKEN, PTOKEN and device cookies are kept and the account is verified before saving.", "Import credential JSON", "Paste one account's JSON, a cookie array or an object containing accessToken/cookie. No file path is read.", "Note", "Credential JSON"),
        "ja" => ("KukuAI にログイン", "デスクトップアプリの公式画面で SMS コードまたは QR ログインを完了してください。この画面の Cookie だけを読み取り、自動でアカウントを追加します。Web・モバイルでは Cookie を貼り付けてください。", "Cookie を貼り付け", "同じ KukuAI セッションの完全な Cookie ヘッダーまたはエディターの JSON を貼り付けます。BDUSS は必須です。STOKEN、PTOKEN、端末 Cookie を保持し、保存前に確認します。", "認証 JSON を取り込む", "単一アカウントの JSON、Cookie 配列、accessToken/cookie を含むオブジェクトを貼り付けます。ファイルパスは読み取りません。", "メモ", "認証 JSON"),
        "ko" => ("KukuAI 로그인", "데스크톱 앱의 공식 창에서 문자 코드 또는 QR 로그인을 완료하세요. 이 창의 쿠키만 읽고 계정을 자동 추가합니다. 웹과 모바일에서는 쿠키를 붙여 넣으세요.", "쿠키 붙여넣기", "한 KukuAI 세션의 전체 Cookie 헤더나 쿠키 편집기 JSON을 붙여 넣으세요. BDUSS가 필요하며 STOKEN, PTOKEN 및 기기 쿠키를 보존하고 저장 전 계정을 확인합니다.", "인증 JSON 가져오기", "계정 하나의 JSON, 쿠키 배열 또는 accessToken/cookie가 포함된 객체를 붙여 넣으세요. 파일 경로를 읽지 않습니다.", "메모", "인증 JSON"),
        "pt-BR" => ("Entrar no KukuAI", "Conclua o login por SMS ou QR na janela oficial do aplicativo desktop. Só os cookies dessa janela são lidos e a conta é adicionada automaticamente. No painel web ou celular, cole os cookies.", "Colar cookies", "Cole o cabeçalho Cookie completo ou um JSON do editor de cookies de uma única sessão KukuAI. BDUSS é obrigatório; STOKEN, PTOKEN e cookies do dispositivo são preservados e a conta é validada antes de salvar.", "Importar JSON de credenciais", "Cole o JSON de uma conta, uma lista de cookies ou um objeto com accessToken/cookie. Nenhum caminho de arquivo é lido.", "Nota", "JSON de credenciais"),
        "zh-Hant" => ("庫庫網頁登入", "請在桌面應用的官方視窗完成手機驗證碼或掃碼登入；只讀取本次視窗的 Cookie 並自動新增帳號。網頁及手機面板請改用貼上 Cookie。", "貼上 Cookie", "接受同一庫庫登入會話的完整 Cookie 標頭或 Cookie 編輯器 JSON；BDUSS 必需，保留 STOKEN、PTOKEN 和裝置 Cookie，儲存前會驗證帳號。", "匯入憑據 JSON", "貼上單帳號 JSON、Cookie 陣列或含 accessToken/cookie 的物件；不會讀取檔案路徑。", "備註名", "憑據 JSON"),
        _ => ("库库网页登录", "请在桌面应用的官方窗口完成手机验证码或扫码登录；只读取本次窗口的 Cookie 并自动添加账号。网页和手机面板请改用粘贴 Cookie。", "粘贴 Cookie", "接受同一库库登录会话的完整 Cookie 头或 Cookie 编辑器 JSON；BDUSS 必需，保留 STOKEN、PTOKEN 和设备 Cookie，保存前会验证账号。", "导入凭据 JSON", "粘贴单账号 JSON、Cookie 数组或含 accessToken/cookie 的对象；不会读取文件路径。", "备注名", "凭据 JSON"),
    };
    let mut desktop = sdk::login::desktop_method(3, "库库 / KukuAI", locale);
    let limitation = match locale {
        "en" => "Encrypted-only client cookies cannot be read; use web sign-in or paste cookies. This import saves a snapshot, not a live link.",
        "ja" => "暗号化された Cookie だけの場合は読み取れません。Web ログインまたは Cookie の貼り付けを使用してください。取り込みはスナップショットで、継続連携ではありません。",
        "ko" => "암호화된 쿠키만 있으면 읽을 수 없습니다. 웹 로그인이나 쿠키 붙여넣기를 사용하세요. 이 가져오기는 실시간 연결이 아닌 스냅샷을 저장합니다.",
        "pt-BR" => "Cookies somente criptografados não podem ser lidos; use login web ou cole os cookies. A importação salva uma cópia, não um vínculo em tempo real.",
        "zh-Hant" => "只有加密 Cookie 時無法讀取，請用網頁登入或貼上 Cookie。匯入保存的是快照，不會持續連結本機檔案。",
        _ => "只有加密 Cookie 时无法读取，请用网页登录或粘贴 Cookie。导入保存的是快照，不会持续关联本机文件。",
    };
    desktop["instructions"] = json!(format!(
        "{}\n{limitation}",
        desktop["instructions"].as_str().unwrap_or("")
    ));
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
