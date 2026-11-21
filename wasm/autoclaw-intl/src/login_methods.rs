use agent2api_wasm_sdk::{json, Value};

pub fn methods(locale: &str) -> Value {
    let text = match locale {
        "en" => ["Sign in with Zai", "Sign in with Google", "Complete authorization in your browser to add the account; when accessing the panel remotely, paste the callback URL after signing in.", "Enter credentials", "Import JSON", "Credentials JSON", "Select or paste an AutoClaw International credential file to import the account; automatic renewal is supported when complete sign-in details are included."],
        "ja" => ["Zai でログイン", "Google でログイン", "ブラウザーで認証を完了するとアカウントを追加できます。リモート画面ではログイン後にコールバック URL を貼り付けてください。", "認証情報を入力", "JSON を取り込む", "認証情報 JSON", "AutoClaw 国際版の認証ファイルを選択または貼り付けてアカウントを取り込めます。完全なログイン情報が含まれていれば自動更新に対応します。"],
        "ko" => ["Zai로 로그인", "Google로 로그인", "브라우저에서 인증을 완료하면 계정이 추가됩니다. 원격 패널에서는 로그인 후 콜백 URL을 붙여넣으세요.", "인증 정보 입력", "JSON 가져오기", "인증 정보 JSON", "AutoClaw 국제판 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있으며, 전체 로그인 정보가 포함되어 있으면 자동 갱신을 지원합니다."],
        "pt-BR" => ["Entrar com Zai", "Entrar com Google", "Conclua a autorização no navegador para adicionar a conta; no acesso remoto ao painel, cole a URL de callback após o login.", "Inserir credenciais", "Importar JSON", "Credenciais JSON", "Selecione ou cole um arquivo de credenciais do AutoClaw International para importar a conta; a renovação automática é suportada quando os dados completos de login estão incluídos."],
        "zh-Hant" => ["Zai 登入", "Google 登入", "在瀏覽器完成授權即可新增帳號；遠端存取面板時，按提示貼上登入後的回呼網址即可。", "手動填寫憑證", "匯入 JSON", "憑證 JSON", "選擇或貼上 AutoClaw 國際版憑據檔案即可匯入帳號；包含完整登入資訊時支援自動續期。"],
        _ => ["Zai 登录", "Google 登录", "在浏览器完成授权即可添加账号；远程访问面板时，按提示粘贴登录后的回调地址即可。", "手动填写凭据", "导入 JSON", "凭据 JSON", "选择或粘贴 AutoClaw 国际版凭据文件即可导入账号；包含完整登录信息时支持自动续期。"],
    };
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": text[0], "prompts": [], "instructions": text[2],
            "browserMode": "choice", "manualCallback": true, "callbackInput": "url", "validatesCallback": true },
        { "methodIndex": 1, "type": "oauth", "label": text[1], "prompts": [], "instructions": text[2],
            "browserMode": "choice", "manualCallback": true, "callbackInput": "url", "validatesCallback": true },
        { "methodIndex": 2, "type": "api", "label": text[3], "prompts": [
            { "key": "token", "type": "text", "label": "Access Token", "secret": true },
            { "key": "refreshToken", "type": "text", "label": "Refresh Token", "secret": true, "optional": true },
            { "key": "deviceId", "type": "text", "label": "Device ID", "optional": true }
        ] },
        { "methodIndex": 3, "type": "api", "label": text[4], "instructions": text[6], "prompts": [
            { "key": "credentialJson", "type": "text", "label": text[5], "secret": true }
        ] },
        agent2api_wasm_sdk::login::desktop_method(4, "AutoClaw International", locale),
        agent2api_wasm_sdk::legacy_credentials::method(5, locale)
    ] })
}
