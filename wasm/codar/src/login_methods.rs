use agent2api_wasm_sdk::{json, Value};

pub fn methods(locale: &str) -> Value {
    let labels = match locale {
        "en" => ["WeChat QR sign-in", "Sign in with SMS", "Paste access token", "Import credentials JSON", "Link local Codar sign-in", "Account note", "Complete the official CAPTCHA before sending SMS. Local import reads only the official client's sign-in files; no extra permission setup is required."],
        "zh-Hant" => ["微信掃碼登入", "手機驗證碼登入", "貼上存取權杖", "匯入憑據 JSON", "關聯本機科搭登入", "備註名稱", "傳送簡訊前需完成人工驗證。本機匯入只讀取官方用戶端登入檔案，無需另行設定權限。"],
        "ja" => ["WeChat QR ログイン", "SMS でログイン", "アクセストークンを貼り付け", "認証 JSON を取り込む", "ローカル Codar ログインを関連付け", "メモ名", "SMS 送信前に公式 CAPTCHA を完了してください。ローカル取り込みは公式クライアントのログインファイルだけを読み取り、追加の権限設定は不要です。"],
        "ko" => ["WeChat QR 로그인", "SMS로 로그인", "액세스 토큰 붙여넣기", "인증 JSON 가져오기", "로컬 Codar 로그인 연결", "메모 이름", "SMS 전송 전에 공식 CAPTCHA를 완료하세요. 로컬 가져오기는 공식 클라이언트 로그인 파일만 읽으며 별도 권한 설정이 필요하지 않습니다."],
        "pt-BR" => ["Entrar pelo QR do WeChat", "Entrar por SMS", "Colar token de acesso", "Importar JSON de credenciais", "Vincular login local do Codar", "Nome da conta", "Conclua o CAPTCHA oficial antes de enviar SMS. A importação local lê apenas os arquivos de login do cliente oficial e não exige permissões extras."],
        _ => ["微信扫码登录", "手机验证码登录", "粘贴访问令牌", "导入凭据 JSON", "关联本机科搭登录态", "备注名", "发送短信前需完成人工验证；本机关联只读取官方客户端当前登录态，无需单独配置权限。"],
    };
    json!({ "instructions": labels[6], "methods": [
        { "methodIndex": 0, "type": "oauth", "label": labels[0], "prompts": [], "browserMode": "none", "manualCallback": false, "instructions": labels[6] },
        { "methodIndex": 1, "type": "oauth", "label": labels[1], "prompts": [], "browserMode": "none", "manualCallback": false, "instructions": labels[6] },
        { "methodIndex": 2, "type": "api", "label": labels[2], "prompts": [
            { "key": "token", "type": "text", "label": "Codar JWT", "secret": true },
            { "key": "name", "type": "text", "label": labels[5], "optional": true }
        ] },
        { "methodIndex": 3, "type": "api", "label": labels[3], "prompts": [
            { "key": "credentialJson", "type": "text", "label": "JSON", "secret": true },
            { "key": "name", "type": "text", "label": labels[5], "optional": true }
        ] },
        agent2api_wasm_sdk::login::desktop_method(4, "科搭 / Codar", locale)
    ] })
}
