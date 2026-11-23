//! 声明式入口由宿主绘制；网页登录只允许宿主托管，手工快照和本机实时来源仍分开。

use agent2api_wasm_sdk::{json, Result, Value};

use crate::errors;

pub fn web_text(locale: &str) -> [&'static str; 2] {
    match locale {
        "en" => ["Sign in to CatPaw", "Complete CatPaw sign-in in the browser to add the account automatically; no callback URL needs to be pasted."],
        "ja" => ["CatPaw にログイン", "ブラウザーで CatPaw にログインすると自動でアカウントが追加されます。コールバック URL の貼り付けは不要です。"],
        "ko" => ["CatPaw 로그인", "브라우저에서 CatPaw 로그인을 완료하면 계정이 자동으로 추가됩니다. 콜백 URL을 붙여넣을 필요가 없습니다."],
        "pt-BR" => ["Entrar no CatPaw", "Conclua o login no CatPaw pelo navegador para adicionar a conta automaticamente; não é necessário colar a URL de callback."],
        "zh-Hant" => ["CatPaw 網頁登入", "在瀏覽器完成 CatPaw 登入即可自動新增帳號，無需貼上回呼網址。"],
        _ => ["CatPaw 网页登录", "在浏览器完成 CatPaw 登录即可自动添加账号，无需粘贴回调地址。"],
    }
}

pub fn methods(locale: &str) -> Value {
    let web = web_text(locale);
    let manual = manual_text(locale);
    let snapshot = snapshot_text(locale);
    let desktop = agent2api_wasm_sdk::login::desktop_method(3, "CatPaw", locale);
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": web[0], "instructions": web[1], "prompts": [],
            "browserMode": "choice", "manualCallback": false, "callbackInput": "url", "validatesCallback": true },
        { "methodIndex": 1, "type": "api", "label": manual[0],
            "instructions": manual[1],
            "prompts": [
                { "key": "token", "type": "text", "label": manual[2], "secret": true },
                { "key": "uid", "type": "text", "label": manual[3], "optional": true },
                { "key": "loginName", "type": "text", "label": manual[4], "optional": true },
                { "key": "name", "type": "text", "label": manual[5], "optional": true }
            ] },
        { "methodIndex": 2, "type": "api", "label": snapshot[0],
            "instructions": snapshot[1],
            "prompts": [
                { "key": "credentialJson", "type": "text", "label": snapshot[2], "secret": true },
                { "key": "name", "type": "text", "label": manual[5], "optional": true }
            ] },
        desktop
    ] })
}

fn manual_text(locale: &str) -> [&'static str; 6] {
    match locale {
        "en" => ["Manual credential snapshot", "Enter the sign-in token and at least one user identifier (UID or sign-in name), or paste auth.json directly.", "Sign-in token / credential JSON", "UID (or sign-in name required)", "Sign-in name", "Account note"],
        "ja" => ["認証情報を手動入力", "ログイントークンと UID またはログイン名のいずれかを入力してください。auth.json をそのまま貼り付けることもできます。", "ログイントークン / 認証情報 JSON", "UID（ログイン名といずれか必須）", "ログイン名", "メモ名"],
        "ko" => ["인증 정보 직접 입력", "로그인 토큰과 UID 또는 로그인 이름 중 하나를 입력하거나 auth.json을 붙여넣으세요.", "로그인 토큰 / 인증 정보 JSON", "UID (로그인 이름과 둘 중 하나 필수)", "로그인 이름", "계정 메모"],
        "pt-BR" => ["Credenciais manuais", "Informe o token de login e pelo menos um identificador (UID ou nome de login), ou cole o auth.json.", "Token de login / JSON de credenciais", "UID (ou nome de login obrigatório)", "Nome de login", "Nota da conta"],
        "zh-Hant" => ["手動憑據快照", "填寫登入權杖，並填寫 uid 或登入名稱其中一項；也可直接貼上 auth.json。", "登入權杖 / 憑據 JSON", "uid（與登入名稱至少填一項）", "登入名稱", "備註名"],
        _ => ["手工凭据快照", "填写登录令牌，并填写 uid 或登录名其中一项；也可直接粘贴 auth.json。", "登录令牌 / 凭据 JSON", "uid（与登录名至少填一项）", "登录名", "备注名"],
    }
}

fn snapshot_text(locale: &str) -> [&'static str; 3] {
    match locale {
        "en" => ["JSON credential snapshot", "Select or paste a CatPaw credential file (such as auth.json) to import the account.", "Credential JSON"],
        "ja" => ["JSON 認証情報のスナップショット", "CatPaw の認証ファイル（auth.json など）を選択または貼り付けてアカウントを取り込めます。", "認証情報 JSON"],
        "ko" => ["JSON 인증 정보 스냅샷", "CatPaw 인증 파일(auth.json 등)을 선택하거나 붙여넣어 계정을 가져올 수 있습니다.", "인증 정보 JSON"],
        "pt-BR" => ["Snapshot JSON de credenciais", "Selecione ou cole um arquivo de credenciais do CatPaw (como auth.json) para importar a conta.", "JSON de credenciais"],
        "zh-Hant" => ["文字 JSON 快照", "選擇或貼上 CatPaw 憑據檔案（如 auth.json）即可匯入帳號。", "憑據 JSON"],
        _ => ["文本 JSON 快照", "选择或粘贴 CatPaw 凭据文件（如 auth.json）即可导入账号。", "凭据 JSON"],
    }
}

pub fn start(params: &Value) -> Result<Value> {
    let index = params["payload"]["methodIndex"].as_u64();
    if index == Some(3) || params["method"] == "desktop" { return crate::desktop::start(params); }
    if matches!(index, Some(1 | 2)) {
        return Err(errors::invalid("CatPaw 手工凭据或文本 JSON 应从添加账号入口提交"));
    }
    crate::web_login::start(params)
}
