//! 声明式登录方法；项目与备注只属于本次提交，不从本机环境或共享设置取值。

use agent2api_wasm_sdk::{json, Value};

pub fn instructions(locale: &str) -> &'static str {
    match locale {
        "en" => "Authorize on Google's official page. If the browser is on another computer, paste the full final callback URL into this login dialog. A Google Cloud project ID is optional; provide your existing project if automatic discovery fails. Google authorization does not bypass Antigravity's account, location or subscription requirements.",
        "zh-Hant" => "在 Google 官方頁面完成授權；瀏覽器不在閘道電腦上時，請在本次登入視窗貼上最終完整回呼網址。Google Cloud 專案 ID 可選填，自動尋找失敗時可指定既有專案。Google 授權不會略過 Antigravity 的帳號、地區或訂閱資格。",
        "ja" => "Google の公式ページで認証してください。別の端末のブラウザーを使う場合は、このログイン画面に最後の完全なコールバック URL を貼り付けます。Google Cloud プロジェクト ID は任意です。自動検出に失敗する場合は既存のプロジェクトを指定できます。Google 認証で Antigravity のアカウント・地域・契約条件を回避することはできません。",
        "ko" => "Google 공식 페이지에서 승인하세요. 브라우저가 게이트웨이 컴퓨터에 없으면 이 로그인 창에 최종 전체 콜백 URL을 붙여넣으세요. Google Cloud 프로젝트 ID는 선택 사항이며 자동 검색에 실패하면 기존 프로젝트를 지정할 수 있습니다. Google 승인은 Antigravity의 계정·지역·구독 조건을 우회하지 않습니다.",
        "pt-BR" => "Autorize na página oficial do Google. Se o navegador estiver em outro computador, cole a URL final completa de retorno nesta tela de login. O ID do projeto Google Cloud é opcional; informe seu projeto existente se a descoberta automática falhar. A autorização do Google não contorna os requisitos de conta, localização ou assinatura do Antigravity.",
        _ => "在 Google 官方页面完成授权；浏览器不在网关电脑上时，请在本次登录窗口粘贴最终完整回调地址。Google Cloud 项目 ID 可选填，自动查找失败时可指定已有项目。Google 授权不会绕过 Antigravity 的账号、地区或订阅资格。",
    }
}

pub fn methods(locale: &str) -> Value {
    let t = match locale {
        "en" => ["Web sign-in", "Paste credentials", "Note", "Optional", "Google Cloud project ID", "Optional account email (identity)", "Paste a Google refresh token. Project and email are optional; keep using the same email when replacing credentials for an existing account."],
        "zh-Hant" => ["網頁登入", "填寫憑證", "備註名", "選填", "Google Cloud 專案 ID", "帳號信箱（選填，用於識別帳號）", "貼上 Google refresh token。專案及信箱可選填；替換既有帳號憑證時請沿用相同信箱。"],
        "ja" => ["ウェブログイン", "認証情報を入力", "メモ", "任意", "Google Cloud プロジェクト ID", "アカウントのメール（任意、識別用）", "Google refresh token を貼り付けます。プロジェクトとメールは任意です。既存アカウントの認証情報を置き換える場合は同じメールを使ってください。"],
        "ko" => ["웹 로그인", "인증 정보 입력", "메모", "선택 사항", "Google Cloud 프로젝트 ID", "계정 이메일 (선택, 계정 식별용)", "Google refresh token을 붙여넣으세요. 프로젝트와 이메일은 선택 사항입니다. 기존 계정의 인증 정보를 바꿀 때는 같은 이메일을 사용하세요."],
        "pt-BR" => ["Entrar pela web", "Colar credenciais", "Nota", "Opcional", "ID do projeto Google Cloud", "E-mail da conta (opcional, identificação)", "Cole um Google refresh token. Projeto e e-mail são opcionais; use o mesmo e-mail ao substituir as credenciais de uma conta existente."],
        _ => ["网页登录", "填写凭据", "备注名", "可选", "Google Cloud 项目 ID", "账号邮箱（选填，用于识别账号）", "粘贴 Google refresh token。项目和邮箱可选填；替换已有账号凭据时请沿用相同邮箱。"],
    };
    let project = json!({ "key": "projectId", "type": "text", "label": t[4], "placeholder": t[3], "optional": true });
    let name = json!({ "key": "name", "type": "text", "label": t[2], "placeholder": t[3], "optional": true });
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": t[0], "instructions": instructions(locale),
            "prompts": [project, name], "manualCallback": true, "callbackInput": "url",
            "validatesCallback": true, "browserMode": "external" },
        { "methodIndex": 1, "type": "api", "label": t[1], "instructions": t[6], "prompts": [
            { "key": "refreshToken", "type": "text", "label": "Google refresh token", "placeholder": "1//…", "secret": true },
            project, { "key": "email", "type": "text", "label": t[5], "placeholder": t[3], "optional": true }, name
        ] }
    ] })
}
