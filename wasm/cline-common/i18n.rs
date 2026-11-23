use agent2api_wasm_sdk::{json, Value};

pub fn locale(value: &str) -> &'static str {
    match value {
        "en" => "en",
        "ja" => "ja",
        "ko" => "ko",
        "pt-BR" => "pt-BR",
        "zh-Hant" => "zh-Hant",
        _ => "zh-CN",
    }
}

pub fn methods(locale: &str) -> Value {
    let locale = self::locale(locale);
    let text = match locale {
        "en" => ["Browser sign-in", "Open the Cline device authorization page and confirm its code. No local callback is required.", "Enter tokens", "Keep the workos: prefix. Without a refresh token, sign in again after expiry.", "Import credentials JSON", "Credentials JSON", "Select or paste a Cline credential file to import the account."],
        "ja" => ["ブラウザーでログイン", "Cline のデバイス認証ページでコードを確認します。本機コールバックは不要です。", "トークンを入力", "workos: 接頭辞を保持します。更新トークンがない場合は期限後に再ログインが必要です。", "認証 JSON を取り込む", "認証情報 JSON", "Cline の認証ファイルを選択または貼り付けてアカウントを取り込めます。"],
        "ko" => ["브라우저 로그인", "Cline 장치 인증 페이지에서 코드를 확인하세요. 로컬 콜백은 필요하지 않습니다.", "토큰 직접 입력", "workos: 접두사를 유지합니다. 갱신 토큰이 없으면 만료 후 다시 로그인해야 합니다.", "인증 JSON 가져오기", "인증 정보 JSON", "Cline 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있습니다."],
        "pt-BR" => ["Entrar pelo navegador", "Abra a autorização de dispositivo do Cline e confirme o código. Não exige retorno local.", "Inserir tokens", "Mantenha o prefixo workos:. Sem refresh token, entre novamente após expirar.", "Importar credenciais JSON", "Credenciais JSON", "Selecione ou cole um arquivo de credenciais do Cline para importar a conta."],
        "zh-Hant" => ["瀏覽器登入", "開啟 Cline 裝置授權頁並確認授權碼，不需要本機回呼。", "手動填寫權杖", "保留 workos: 前綴；沒有續期權杖時，到期後需重新登入。", "匯入憑證 JSON", "憑證 JSON", "選擇或貼上 Cline 憑據檔案即可匯入帳號。"],
        _ => ["浏览器登录", "打开 Cline 设备授权页并确认授权码，不需要本机回调。", "手动填写令牌", "保留 workos: 前缀；没有续期令牌时，到期后需重新登录。", "导入凭证 JSON", "凭证 JSON", "选择或粘贴 Cline 凭据文件即可导入账号。"],
    };
    let desktop = agent2api_wasm_sdk::login::desktop_method(3, "Cline", locale);
    let methods = vec![
        json!({ "methodIndex": 0, "type": "oauth", "label": text[0], "instructions": text[1], "prompts": [], "browserMode": "choice", "manualCallback": false }),
        json!({ "methodIndex": 1, "type": "api", "label": text[2], "instructions": text[3], "prompts": [
            { "key": "accessToken", "type": "text", "label": "Access Token", "secret": true },
            { "key": "refreshToken", "type": "text", "label": "Refresh Token", "secret": true, "optional": true }
        ] }),
        json!({ "methodIndex": 2, "type": "api", "label": text[4], "instructions": text[6], "prompts": [
            { "key": "credentialJson", "type": "text", "label": text[5], "secret": true }
        ] }),
        desktop,
    ];
    json!({ "methods": methods })
}

pub fn login_prompt(locale: &str, code: &str, seconds: i64) -> String {
    match self::locale(locale) {
        "en" => format!("Enter {code} on the Cline authorization page and confirm within {seconds} seconds. The account will then be returned automatically."),
        "ja" => format!("Cline の認証ページで {code} を入力し、{seconds} 秒以内に確認してください。アカウントを自動的に取得します。"),
        "ko" => format!("Cline 인증 페이지에 {code}를 입력하고 {seconds}초 안에 확인하세요. 계정이 자동으로 추가됩니다."),
        "pt-BR" => format!("Digite {code} na página de autorização do Cline e confirme em {seconds} segundos. A conta será recebida automaticamente."),
        "zh-Hant" => format!("在 Cline 授權頁輸入 {code} 並確認（{seconds} 秒內有效），隨後自動取回帳號憑證"),
        _ => format!("在 Cline 授权页输入 {code} 并确认（{seconds} 秒内有效），随后自动取回账号凭据"),
    }
}

pub fn login_pending(locale: &str) -> &'static str {
    match self::locale(locale) {
        "en" => "Waiting for Cline authorization in your browser",
        "ja" => "ブラウザーで Cline の認証を完了するまで待機中",
        "ko" => "브라우저에서 Cline 인증을 완료하기를 기다리는 중",
        "pt-BR" => "Aguardando a autorização do Cline no navegador",
        "zh-Hant" => "等待在瀏覽器完成 Cline 授權",
        _ => "等待在浏览器完成 Cline 授权",
    }
}

pub fn login_failure(locale: &str, kind: &str) -> &'static str {
    let index = match kind {
        "expired" | "expired_token" => 0,
        "canceled" | "access_denied" => 1,
        "unknown" => 2,
        "register" => 3,
        _ => 4,
    };
    let messages = match self::locale(locale) {
        "en" => ["Cline authorization expired. Start a new sign-in.", "Cline sign-in was canceled or refused. Start a new sign-in.", "The result of this Cline sign-in step is unconfirmed. It will not be submitted again; start a new sign-in.", "Cline did not confirm the account token registration. Start a new sign-in; the exchange will not be repeated.", "Cline sign-in could not be completed. Check your connection and start a new sign-in."],
        "ja" => ["Cline の認証期限が切れました。ログインをやり直してください。", "Cline のログインが取り消されたか拒否されました。やり直してください。", "今回の Cline 認証結果を確認できません。再送しないため、新しいログインを開始してください。", "Cline のアカウントトークン登録を確認できません。再交換せず、ログインをやり直してください。", "Cline のログインを完了できません。接続を確認し、やり直してください。"],
        "ko" => ["Cline 인증이 만료되었습니다. 새 로그인을 시작하세요.", "Cline 로그인이 취소되거나 거절되었습니다. 새 로그인을 시작하세요.", "이번 Cline 로그인 단계의 결과를 확인할 수 없습니다. 다시 제출하지 않으므로 새 로그인을 시작하세요.", "Cline 계정 토큰 등록이 확인되지 않았습니다. 교환을 반복하지 않으므로 새 로그인을 시작하세요.", "Cline 로그인을 완료하지 못했습니다. 연결을 확인하고 다시 로그인하세요."],
        "pt-BR" => ["A autorização do Cline expirou. Inicie um novo login.", "O login do Cline foi cancelado ou recusado. Inicie um novo login.", "O resultado desta etapa não foi confirmado. Ela não será reenviada; inicie um novo login.", "O Cline não confirmou o registro do token. Inicie um novo login; a troca não será repetida.", "Não foi possível concluir o login do Cline. Verifique a conexão e inicie um novo login."],
        "zh-Hant" => ["Cline 裝置授權已過期，請重新發起登入", "Cline 登入已取消或授權被拒絕，請重新發起", "Cline 本次登入步驟結果未確認，不會再次提交；請重新發起登入", "Cline 未確認本次帳號權杖登記，不會重複交換；請重新發起登入", "Cline 登入未完成，請檢查網路後重新發起"],
        _ => ["Cline 设备授权已过期，请重新发起登录", "Cline 登录已取消或授权被拒绝，请重新发起", "Cline 本次登录步骤结果未确认，不会再次提交；请重新发起登录", "Cline 未确认本次账号令牌登记，不会重复兑换；请重新发起登录", "Cline 登录未完成，请检查网络后重新发起"],
    };
    messages[index]
}
