use agent2api_wasm_sdk::{json, Value};

pub fn methods(locale: &str) -> Value {
    let text = match locale {
        "en" => [
            "Paste CodeArts credential JSON",
            "Select or paste a CodeArts credential file to import the account; automatic renewal is supported when complete sign-in details are included.",
            "Complete credential JSON",
            "Account note",
            "CodeArts web sign-in",
            "Sign in on the official Huawei Cloud page to add the account; when accessing the panel remotely, paste the callback URL after signing in.",
        ],
        "zh-Hant" => [
            "貼上 CodeArts 憑據 JSON",
            "選擇或貼上 CodeArts 憑據檔案即可匯入帳號；包含完整登入資訊時支援自動續期。",
            "完整憑據 JSON",
            "備註名",
            "CodeArts 網頁登入",
            "在華為雲官方頁面完成登入即可新增帳號；遠端存取面板時，按提示貼上登入後的回呼網址即可。",
        ],
        "ja" => [
            "CodeArts 認証 JSON を貼り付け",
            "CodeArts の認証ファイルを選択または貼り付けてアカウントを取り込めます。完全なログイン情報が含まれていれば自動更新に対応します。",
            "完全な認証 JSON",
            "メモ名",
            "CodeArts Web ログイン",
            "Huawei Cloud の公式ページでログインするとアカウントを追加できます。リモート画面から操作する場合は、ログイン後にコールバック URL を貼り付けてください。",
        ],
        "ko" => [
            "CodeArts 자격 증명 JSON 붙여넣기",
            "CodeArts 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있으며, 전체 로그인 정보가 포함되어 있으면 자동 갱신을 지원합니다.",
            "완전한 자격 증명 JSON",
            "계정 메모",
            "CodeArts 웹 로그인",
            "Huawei Cloud 공식 페이지에서 로그인하면 계정이 추가됩니다. 원격 패널에서 접속한 경우 로그인 후 콜백 URL을 붙여넣으세요.",
        ],
        "pt-BR" => [
            "Colar credenciais JSON do CodeArts",
            "Selecione ou cole um arquivo de credenciais do CodeArts para importar a conta; a renovação automática é suportada quando os dados completos de login estão incluídos.",
            "Credenciais JSON completas",
            "Nome de observação",
            "Login web do CodeArts",
            "Conclua o login na página oficial da Huawei Cloud para adicionar a conta; no acesso remoto ao painel, cole a URL de callback após o login.",
        ],
        _ => [
            "粘贴 CodeArts 凭据 JSON",
            "选择或粘贴 CodeArts 凭据文件即可导入账号；包含完整登录信息时支持自动续期。",
            "完整凭据 JSON",
            "备注名",
            "CodeArts 网页登录",
            "在华为云官方页面完成登录即可添加账号；远程访问面板时，按提示粘贴登录后的回调地址即可。",
        ],
    };
    json!({ "methods": [
        { "methodIndex": 0, "type": "api", "label": text[0], "instructions": text[1],
            "prompts": [
                { "key": "credentialJson", "type": "text", "label": text[2], "secret": true },
                { "key": "name", "type": "text", "label": text[3], "optional": true }
            ] },
        { "methodIndex": 1, "type": "oauth", "label": text[4], "prompts": [],
            "browserMode": "choice", "manualCallback": true, "callbackInput": "url", "validatesCallback": true,
            "instructions": text[5] }
    ] })
}
