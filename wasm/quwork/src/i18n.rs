use agent2api_wasm_sdk::{json, Value};

pub fn methods(locale: &str) -> Value {
    let text = match locale {
        "en" => ["Import local QuWork sign-in", "Reads only the official QuWork files on the connected gateway computer. Saves a live reference without changing client files; no extra helper installation or permission setup. Import again after switching accounts.",
            "QuWork credentials", "Paste complete credentials JSON or the llmToken alone. Only the model token is needed for chat; balance, account renewal and rewards require accessToken. Not validated with a real account.",
            "Credentials JSON or model token", "Account accessToken", "Optional when included in the JSON", "Original QuWork device identifier", "Optional; required for redemption-code activation. Never generated automatically."],
        "ja" => ["QuWork のローカルログインを取り込む", "接続先ゲートウェイのコンピューターにある公式 QuWork のファイルだけを読み取り、参照を保存します。公式ファイルの変更、補助プラグインのインストールや追加の権限設定は不要です。アカウント変更後は再取り込みが必要です。",
            "QuWork 認証情報", "完全な認証 JSON または llmToken のみを貼り付けます。残高、アカウント更新、特典には accessToken が必要です。実アカウントでは未検証です。",
            "認証 JSON またはモデルトークン", "アカウント accessToken", "JSON に含まれる場合は省略可", "元の QuWork デバイス識別子", "任意。交換コードの有効化時に必要です。自動生成しません。"],
        "ko" => ["QuWork 로컬 로그인 가져오기", "연결된 게이트웨이 컴퓨터의 공식 QuWork 파일만 읽고 참조를 저장합니다. 공식 파일을 수정하지 않으며 추가 도우미 설치나 권한 설정은 필요하지 않습니다. 계정 변경 후 다시 가져오세요.",
            "QuWork 인증 정보", "전체 인증 JSON 또는 llmToken만 붙여넣으세요. 잔액, 계정 갱신 및 보상에는 accessToken이 필요합니다. 실제 계정으로 검증하지 않았습니다.",
            "인증 JSON 또는 모델 토큰", "계정 accessToken", "JSON에 포함된 경우 생략 가능", "원래 QuWork 장치 식별자", "선택 사항. 교환 코드 활성화에 필요하며 자동 생성하지 않습니다."],
        "pt-BR" => ["Importar login local do QuWork", "Lê apenas os arquivos oficiais do QuWork no computador do gateway conectado e salva uma referência. Não altera arquivos e não exige instalar auxiliares ou configurar permissões extras. Importe novamente após trocar de conta.",
            "Credenciais QuWork", "Cole o JSON completo ou apenas llmToken. Saldo, renovação da conta e recompensas exigem accessToken. Não validado com conta real.",
            "JSON de credenciais ou token do modelo", "accessToken da conta", "Opcional quando incluído no JSON", "Identificador original do dispositivo QuWork", "Opcional; necessário para ativar códigos de resgate. Nunca é gerado automaticamente."],
        "zh-Hant" => ["匯入 QuWork 本機登入態", "只讀取所連接閘道電腦的官方 QuWork 檔案並儲存引用，不修改用戶端檔案，也不需要額外安裝或授權輔助外掛；切換帳號後請重新匯入。",
            "QuWork 憑證", "貼上完整憑證 JSON 或僅模型憑證 llmToken。積分、帳號續期及獎勵需 accessToken。尚未使用真實帳號驗證。",
            "憑證 JSON 或模型憑證", "登入憑證 accessToken", "完整 JSON 已包含時無需填寫", "QuWork 原裝置識別碼", "選填，兌換碼啟用時需要；不會自動生成。"],
        _ => ["导入 QuWork 本机登录态", "只读官方客户端当前登录态；只保存本机引用，不修改客户端文件，不需要单独配置权限。",
            "QuWork 凭证", "粘贴完整凭证 JSON 或仅模型凭证 llmToken。积分、账号续期及奖励需 accessToken。尚未使用真实账号验证。",
            "凭证 JSON 或模型凭证", "登录凭证 accessToken", "完整 JSON 已包含时无需填写", "QuWork 原设备标识", "可选，兑换码激活时需要；不会自动生成。"],
    };
    json!({ "methods": [
        local_method(locale),
        { "methodIndex": 1, "type": "api", "label": text[2], "instructions": text[3], "prompts": [
            { "key": "credentials", "type": "text", "label": text[4], "placeholder": "{\"accessToken\":\"…\",\"llmToken\":\"…\"}", "secret": true },
            { "key": "accessToken", "type": "text", "label": text[5], "placeholder": text[6], "secret": true, "optional": true },
            { "key": "deviceIdentifier", "type": "text", "label": text[7], "placeholder": text[8], "secret": true, "optional": true }
          ], "localOnly": false, "manualCallback": false, "callbackInput": "url" }
    ] })
}

fn local_method(locale: &str) -> Value {
    let mut method = agent2api_wasm_sdk::login::desktop_method(0, "QuWork", locale);
    let note = match locale {
        "en" => " Saves a live reference, not a credential snapshot. Import again after switching accounts. Only the model token is needed for chat; account renewal uses the existing account-session token, not a separate refresh token.",
        "ja" => " 認証情報のコピーではなく参照を保存します。アカウント変更後は再取り込みしてください。対話にはモデルトークンだけが必要です。アカウント更新には既存のログイントークンを使い、別の更新トークンは使いません。",
        "ko" => " 자격 증명 복사본이 아닌 참조를 저장합니다. 계정을 바꾸면 다시 가져오세요. 채팅에는 모델 토큰만 필요하며 계정 갱신은 별도 새로 고침 토큰이 아닌 기존 로그인 토큰을 사용합니다.",
        "pt-BR" => " Salva uma referência, não uma cópia das credenciais. Importe novamente após trocar de conta. O chat exige só o token do modelo; a renovação usa o token da sessão da conta, não um refresh token separado.",
        "zh-Hant" => " 儲存的是即時引用而不是憑證快照；官方用戶端切換帳號後請重新匯入。對話只需模型憑證，帳號續期沿用既有登入會話，不使用獨立的 refreshToken。",
        _ => " 保存的是实时引用而不是凭据快照；官方客户端切换账号后请重新导入。对话只需模型凭证，账号续期沿用既有登录会话，不使用独立的 refreshToken。",
    };
    method["instructions"] = json!(format!(
        "{}{note}",
        method["instructions"].as_str().unwrap_or("")
    ));
    method
}
