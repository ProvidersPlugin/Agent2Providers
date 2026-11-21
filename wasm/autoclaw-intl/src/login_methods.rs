use agent2api_wasm_sdk::{json, Value};

pub fn methods(locale: &str) -> Value {
    let text = match locale {
        "en" => ["Sign in with Zai", "Sign in with Google", "Complete official Aliyun verification, then authorize in your browser. For a remote gateway, paste the complete callback address. Local session renewal requires separate permission in plugin settings.", "Enter credentials", "Import JSON", "Credentials JSON"],
        "ja" => ["Zai でログイン", "Google でログイン", "公式 Aliyun の本人確認後、ブラウザーで認証します。リモート接続では完全なコールバック URL を貼り付けてください。本機の更新にはプラグイン設定で別途許可が必要です。", "認証情報を入力", "JSON を取り込む", "認証情報 JSON"],
        "ko" => ["Zai로 로그인", "Google로 로그인", "공식 Aliyun 인증 후 브라우저에서 승인하세요. 원격 게이트웨이에서는 전체 콜백 주소를 붙여 넣으세요. 로컬 세션 갱신은 플러그인 설정에서 별도로 승인해야 합니다.", "인증 정보 입력", "JSON 가져오기", "인증 정보 JSON"],
        "pt-BR" => ["Entrar com Zai", "Entrar com Google", "Conclua a verificação oficial Aliyun e autorize no navegador. Para um gateway remoto, cole o endereço completo de retorno. A renovação da sessão local exige permissão separada nas configurações do plugin.", "Inserir credenciais", "Importar JSON", "Credenciais JSON"],
        "zh-Hant" => ["Zai 登入", "Google 登入", "完成官方阿里雲驗證後，在瀏覽器授權；遠端連線可貼上完整回呼網址。本機登入態續期需在插件設定中另行授權。", "手動填寫憑證", "匯入 JSON", "憑證 JSON"],
        _ => ["Zai 登录", "Google 登录", "完成官方阿里云验证后，在浏览器授权；远程连接可粘贴完整回调地址。本机登录态续期需在插件设置中另行授权。", "手动填写凭据", "导入 JSON", "凭据 JSON"],
    };
    json!({ "instructions": text[2], "methods": [
        { "methodIndex": 0, "type": "oauth", "label": text[0], "prompts": [], "instructions": text[2],
            "browserMode": "choice", "manualCallback": true, "callbackInput": "url", "validatesCallback": true },
        { "methodIndex": 1, "type": "oauth", "label": text[1], "prompts": [], "instructions": text[2],
            "browserMode": "choice", "manualCallback": true, "callbackInput": "url", "validatesCallback": true },
        { "methodIndex": 2, "type": "api", "label": text[3], "prompts": [
            { "key": "token", "type": "text", "label": "Access Token", "secret": true },
            { "key": "refreshToken", "type": "text", "label": "Refresh Token", "secret": true, "optional": true },
            { "key": "deviceId", "type": "text", "label": "Device ID", "optional": true }
        ] },
        { "methodIndex": 3, "type": "api", "label": text[4], "prompts": [
            { "key": "credentialJson", "type": "text", "label": text[5], "secret": true }
        ] },
        agent2api_wasm_sdk::login::desktop_method(4, "AutoClaw International", locale),
        agent2api_wasm_sdk::legacy_credentials::method(5, locale)
    ] })
}
