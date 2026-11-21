//! 声明式登录方法；项目与备注只属于本次提交，不从本机环境或共享设置取值。

use agent2api_wasm_sdk::{json, Value};

pub fn instructions(locale: &str) -> &'static str {
    match locale {
        "en" => "Sign in on the official Google page to add the account; when accessing the panel remotely, paste the callback URL after signing in.",
        "zh-Hant" => "在 Google 官方頁面完成登入即可新增帳號；遠端存取面板時，按提示貼上登入後的回呼網址即可。",
        "ja" => "Google の公式ページでログインするとアカウントを追加できます。リモート画面ではログイン後にコールバック URL を貼り付けてください。",
        "ko" => "Google 공식 페이지에서 로그인하면 계정이 추가됩니다. 원격 패널에서는 로그인 후 콜백 URL을 붙여넣으세요.",
        "pt-BR" => "Conclua o login na página oficial do Google para adicionar a conta; no acesso remoto ao painel, cole a URL de callback após o login.",
        _ => "在 Google 官方页面完成登录即可添加账号；远程访问面板时，按提示粘贴登录后的回调地址即可。",
    }
}

pub fn methods(locale: &str) -> Value {
    let t = match locale {
        "en" => ["Web sign-in", "Paste credentials", "Note", "Optional", "Google Cloud project ID", "Optional account email (identity)", "Paste a Google refresh token to add the account; project ID and email are optional."],
        "zh-Hant" => ["網頁登入", "填寫憑證", "備註名", "選填", "Google Cloud 專案 ID", "帳號信箱（選填，用於識別帳號）", "貼上 Google Refresh Token 即可新增帳號；專案 ID 與信箱為選填項。"],
        "ja" => ["ウェブログイン", "認証情報を入力", "メモ", "任意", "Google Cloud プロジェクト ID", "アカウントのメール（任意、識別用）", "Google Refresh Token を貼り付けてアカウントを追加できます。プロジェクト ID とメールは任意です。"],
        "ko" => ["웹 로그인", "인증 정보 입력", "메모", "선택 사항", "Google Cloud 프로젝트 ID", "계정 이메일 (선택, 계정 식별용)", "Google Refresh Token을 붙여넣어 계정을 추가할 수 있으며, 프로젝트 ID와 이메일은 선택 사항입니다."],
        "pt-BR" => ["Entrar pela web", "Colar credenciais", "Nota", "Opcional", "ID do projeto Google Cloud", "E-mail da conta (opcional, identificação)", "Cole um Google Refresh Token para adicionar a conta; ID do projeto e e-mail são opcionais."],
        _ => ["网页登录", "填写凭据", "备注名", "可选", "Google Cloud 项目 ID", "账号邮箱（选填，用于识别账号）", "粘贴 Google Refresh Token 即可添加账号；项目 ID 与邮箱为选填项。"],
    };
    let project = json!({ "key": "projectId", "type": "text", "label": t[4], "placeholder": t[3], "optional": true });
    let name = json!({ "key": "name", "type": "text", "label": t[2], "placeholder": t[3], "optional": true });
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": t[0], "instructions": instructions(locale),
            "prompts": [project, name], "manualCallback": true, "callbackInput": "url",
            "validatesCallback": true, "browserMode": "choice" },
        { "methodIndex": 1, "type": "api", "label": t[1], "instructions": t[6], "prompts": [
            { "key": "refreshToken", "type": "text", "label": "Google refresh token", "placeholder": "1//…", "secret": true },
            project, { "key": "email", "type": "text", "label": t[5], "placeholder": t[3], "optional": true }, name
        ] }
    ] })
}
