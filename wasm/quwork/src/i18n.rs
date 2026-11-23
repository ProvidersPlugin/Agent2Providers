use agent2api_wasm_sdk::{json, Value};

pub fn methods(locale: &str) -> Value {
    let text = match locale {
        "en" => ["Import local QuWork sign-in", "Imports the signed-in QuWork account from the gateway computer.",
            "QuWork credentials", "Select or paste a QuWork credential file or model token (llmToken); include accessToken for balance, renewal and rewards.",
            "Credentials JSON or model token", "Account accessToken", "Optional when included in the JSON", "Original QuWork device identifier", "Optional; required for redemption-code activation."],
        "ja" => ["QuWork のローカルログインを取り込む", "ゲートウェイのコンピューターでログイン済みの QuWork アカウントを取り込みます。",
            "QuWork 認証情報", "QuWork の認証ファイルまたは llmToken を選択・貼り付けてください。残高照会、更新、特典には accessToken が必要です。",
            "認証 JSON またはモデルトークン", "アカウント accessToken", "JSON に含まれる場合は省略可", "元の QuWork デバイス識別子", "任意（交換コードの有効化時に必要）"],
        "ko" => ["QuWork 로컬 로그인 가져오기", "게이트웨이 컴퓨터에 로그인된 QuWork 계정을 가져옵니다.",
            "QuWork 인증 정보", "QuWork 인증 파일 또는 llmToken을 선택하거나 붙여넣으세요. 잔액 조회, 갱신 및 보상에는 accessToken이 필요합니다.",
            "인증 JSON 또는 모델 토큰", "계정 accessToken", "JSON에 포함된 경우 생략 가능", "원래 QuWork 장치 식별자", "선택 사항 (교환 코드 활성화에 필요)"],
        "pt-BR" => ["Importar login local do QuWork", "Importa a conta conectada no QuWork do computador do gateway.",
            "Credenciais QuWork", "Selecione ou cole um arquivo de credenciais do QuWork ou llmToken; inclua accessToken para saldo, renovação e recompensas.",
            "JSON de credenciais ou token do modelo", "accessToken da conta", "Opcional quando incluído no JSON", "Identificador original do dispositivo QuWork", "Opcional; necessário para ativar códigos de resgate."],
        "zh-Hant" => ["匯入 QuWork 本機登入態", "讀取閘道所在電腦上已登入的 QuWork 帳號。",
            "QuWork 憑證", "選擇或貼上 QuWork 憑據檔案或模型憑證（llmToken）；填寫 accessToken 後支援積分查詢、自動續期及獎勵兌換。",
            "憑證 JSON 或模型憑證", "登入憑證 accessToken", "完整 JSON 已包含時無需填寫", "QuWork 原裝置識別碼", "選填，兌換碼啟用時需要"],
        _ => ["导入 QuWork 本机登录态", "读取网关所在电脑上已登录的 QuWork 账号。",
            "QuWork 凭证", "选择或粘贴 QuWork 凭据文件或模型凭证（llmToken）；填写 accessToken 后支持积分查询、自动续期及奖励兑换。",
            "凭证 JSON 或模型凭证", "登录凭证 accessToken", "完整 JSON 已包含时无需填写", "QuWork 原设备标识", "可选，兑换码激活时需要"],
    };
    json!({ "methods": [
        agent2api_wasm_sdk::login::desktop_method(0, "QuWork", locale),
        { "methodIndex": 1, "type": "api", "label": text[2], "instructions": text[3], "prompts": [
            { "key": "credentials", "type": "text", "label": text[4], "placeholder": "{\"accessToken\":\"…\",\"llmToken\":\"…\"}", "secret": true },
            { "key": "accessToken", "type": "text", "label": text[5], "placeholder": text[6], "secret": true, "optional": true },
            { "key": "deviceIdentifier", "type": "text", "label": text[7], "placeholder": text[8], "secret": true, "optional": true }
          ], "localOnly": false, "manualCallback": false, "callbackInput": "url" }
    ] })
}
