use agent2api_wasm_sdk::{json, Value};

pub fn methods(locale: &str) -> Value {
    let labels = match locale {
        "en" => ["WeChat QR sign-in", "Sign in with SMS", "Paste access token", "Import credentials JSON", "Link local Codar sign-in", "Account note", "Scan the QR code with WeChat to sign in.", "Complete verification and enter the SMS code to sign in.", "Select or paste a Codar credential file to import the account."],
        "zh-Hant" => ["微信掃碼登入", "手機驗證碼登入", "貼上存取權杖", "匯入憑據 JSON", "關聯本機科搭登入", "備註名稱", "使用微信掃碼即可登入並新增帳號。", "完成驗證並輸入簡訊驗證碼即可登入。", "選擇或貼上科搭憑據檔案即可匯入帳號。"],
        "ja" => ["WeChat QR ログイン", "SMS でログイン", "アクセストークンを貼り付け", "認証 JSON を取り込む", "ローカル Codar ログインを関連付け", "メモ名", "WeChat で QR コードを読み取ってログインします。", "確認を完了し、SMS 認証コードを入力してログインします。", "Codar の認証ファイルを選択または貼り付けてアカウントを取り込めます。"],
        "ko" => ["WeChat QR 로그인", "SMS로 로그인", "액세스 토큰 붙여넣기", "인증 JSON 가져오기", "로컬 Codar 로그인 연결", "메모 이름", "WeChat으로 QR 코드를 스캔하여 로그인합니다.", "인증을 완료하고 SMS 인증 코드를 입력해 로그인합니다.", "Codar 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있습니다."],
        "pt-BR" => ["Entrar pelo QR do WeChat", "Entrar por SMS", "Colar token de acesso", "Importar JSON de credenciais", "Vincular login local do Codar", "Nome da conta", "Leia o QR code com o WeChat para entrar.", "Conclua a verificação e informe o código SMS para entrar.", "Selecione ou cole um arquivo de credenciais do Codar para importar a conta."],
        _ => ["微信扫码登录", "手机验证码登录", "粘贴访问令牌", "导入凭据 JSON", "关联本机科搭登录态", "备注名", "使用微信扫码即可登录并添加账号。", "完成验证并输入短信验证码即可登录。", "选择或粘贴科搭凭据文件即可导入账号。"],
    };
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": labels[0], "prompts": [], "browserMode": "none", "manualCallback": false, "instructions": labels[6] },
        { "methodIndex": 1, "type": "oauth", "label": labels[1], "prompts": [], "browserMode": "none", "manualCallback": false, "instructions": labels[7] },
        { "methodIndex": 2, "type": "api", "label": labels[2], "prompts": [
            { "key": "token", "type": "text", "label": "Codar JWT", "secret": true },
            { "key": "name", "type": "text", "label": labels[5], "optional": true }
        ] },
        { "methodIndex": 3, "type": "api", "label": labels[3], "instructions": labels[8], "prompts": [
            { "key": "credentialJson", "type": "text", "label": "JSON", "secret": true },
            { "key": "name", "type": "text", "label": labels[5], "optional": true }
        ] },
        agent2api_wasm_sdk::login::desktop_method(4, "科搭 / Codar", locale)
    ] })
}
