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
        "en" => ["Browser sign-in", "Open the Cline device authorization page and confirm its code. No local callback is required.", "Enter tokens", "Keep the workos: prefix. Without a refresh token, sign in again after expiry.", "Import credentials JSON", "Credentials JSON", "Paste a Cline account JSON or providers.json. Only the Cline auth section is imported; other providers are ignored."],
        "ja" => ["ブラウザーでログイン", "Cline のデバイス認証ページでコードを確認します。本機コールバックは不要です。", "トークンを入力", "workos: 接頭辞を保持します。更新トークンがない場合は期限後に再ログインが必要です。", "認証 JSON を取り込む", "認証情報 JSON", "Cline アカウント JSON または providers.json を貼り付けます。Cline の認証部分だけを取り込みます。"],
        "ko" => ["브라우저 로그인", "Cline 장치 인증 페이지에서 코드를 확인하세요. 로컬 콜백은 필요하지 않습니다.", "토큰 직접 입력", "workos: 접두사를 유지합니다. 갱신 토큰이 없으면 만료 후 다시 로그인해야 합니다.", "인증 JSON 가져오기", "인증 정보 JSON", "Cline 계정 JSON 또는 providers.json을 붙여넣으세요. Cline 인증 부분만 가져옵니다."],
        "pt-BR" => ["Entrar pelo navegador", "Abra a autorização de dispositivo do Cline e confirme o código. Não exige retorno local.", "Inserir tokens", "Mantenha o prefixo workos:. Sem refresh token, entre novamente após expirar.", "Importar credenciais JSON", "Credenciais JSON", "Cole um JSON de conta Cline ou providers.json. Apenas a seção de autenticação do Cline é importada."],
        "zh-Hant" => ["瀏覽器登入", "開啟 Cline 裝置授權頁並確認授權碼，不需要本機回呼。", "手動填寫權杖", "保留 workos: 前綴；沒有續期權杖時，到期後需重新登入。", "匯入憑證 JSON", "憑證 JSON", "貼上單一 Cline 帳號 JSON 或 providers.json，只匯入 Cline auth，不讀其他提供商。"],
        _ => ["浏览器登录", "打开 Cline 设备授权页并确认授权码，不需要本机回调。", "手动填写令牌", "保留 workos: 前缀；没有续期令牌时，到期后需重新登录。", "导入凭证 JSON", "凭证 JSON", "粘贴单个 Cline 账号 JSON 或 providers.json，只导入 Cline auth，不读取其他提供商。"],
    };
    let free = crate::PROVIDER_ID == "cline-free";
    let renewal = match locale {
        "en" => "Ordinary sign-in and imports do not grant renewal rights automatically. New Free accounts can use explicit enrollment; existing accounts can update credentials in account settings and explicitly authorize renewal. Local links require separate local-renewal permission in plugin settings.",
        "ja" => "通常のログインや取り込みでは更新権限を自動で付与しません。新しい無料枠アカウントは明示的に登録し、既存アカウントは設定で新しい認証情報を保存して更新を許可できます。本機の参照はプラグイン設定で別途更新許可が必要です。",
        "ko" => "일반 로그인과 가져오기는 자동 갱신 권한을 부여하지 않습니다. 새 무료 풀 계정은 명시적으로 등록하고, 기존 계정은 계정 설정에서 새 인증 정보를 저장하며 갱신을 허용할 수 있습니다. 로컬 연결은 플러그인 설정에서 별도 갱신 허용이 필요합니다.",
        "pt-BR" => "Login e importações comuns não concedem renovação automaticamente. Novas contas Free podem ser cadastradas com confirmação; contas existentes podem atualizar credenciais e autorizar renovação nas configurações da conta. Vínculos locais exigem permissão separada no plugin.",
        "zh-Hant" => "一般登入與匯入不會自動補續期權限。新免費池帳號可明確登記；已有帳號可在帳號設定中更新新憑據並勾選續期授權。本機即時引用需在插件設定中另行授權本機續期。",
        _ => "普通登录和导入不会自动补续期权限。新免费池账号可明确登记；已有账号可在账号设置中更新新凭据并勾选续期授权。本机实时引用需在插件设置中另行授权本机续期。",
    };
    let readonly = match locale {
        "en" => "Stores a live local reference, not a token snapshot. Local renewal and writing rotated credentials back require separate permission in plugin settings; otherwise, sign in again in the official client.",
        "ja" => "トークンのコピーではなく本機ログインの参照を保存します。本機の更新と認証情報の書き戻しにはプラグイン設定で別途許可が必要です。許可しない場合は公式クライアントで再ログインしてください。",
        "ko" => "토큰 사본이 아닌 로컬 로그인 참조를 저장합니다. 로컬 갱신과 새 인증 정보 저장은 플러그인 설정에서 별도로 허용해야 합니다. 허용하지 않으면 공식 클라이언트에 다시 로그인하세요.",
        "pt-BR" => "Salva uma referência ao login local, não uma cópia dos tokens. Renovar e gravar credenciais atualizadas exige permissão separada no plugin; sem ela, entre novamente no cliente oficial.",
        "zh-Hant" => "保存本機即時引用，不保存令牌快照。本機續期及寫回新憑據需在插件設定中另外授權；未授權時請在官方用戶端重新登入。",
        _ => "保存本机实时引用，不保存令牌快照。本机续期及写回新凭据需在插件设置中另外授权；未授权时请在官方客户端重新登录。",
    };
    let mut desktop = agent2api_wasm_sdk::login::desktop_method(3, "Cline", locale);
    desktop["instructions"] = json!(format!(
        "{}\n{readonly}",
        desktop["instructions"].as_str().unwrap_or("")
    ));
    let key_text = match locale {
        "en" => ["Cline API Key", "Paste the original API key from app.cline.bot → Settings → API Keys. It is sent without the workos: prefix and is not renewed as an OAuth token.", "API Key"],
        "ja" => ["Cline API キー", "app.cline.bot の Settings → API Keys で発行したキーを貼り付けます。workos: 接頭辞を付けずに送信し、OAuth トークンとして更新しません。", "API キー"],
        "ko" => ["Cline API 키", "app.cline.bot의 Settings → API Keys에서 발급한 원본 키를 붙여넣으세요. workos: 접두사 없이 전송하며 OAuth 토큰처럼 갱신하지 않습니다.", "API 키"],
        "pt-BR" => ["Chave de API do Cline", "Cole a chave original de app.cline.bot → Settings → API Keys. Ela é enviada sem o prefixo workos: e não é renovada como um token OAuth.", "Chave de API"],
        "zh-Hant" => ["Cline API Key", "貼上 app.cline.bot → Settings → API Keys 建立的原始密鑰，不加 workos: 前綴，也不按 OAuth 權杖自動續期。", "API Key"],
        _ => ["Cline API Key", "粘贴 app.cline.bot → Settings → API Keys 创建的原始密钥，不加 workos: 前缀，也不按 OAuth 令牌自动续期。", "API Key"],
    };
    let instruction = |base: &str| {
        if free {
            format!("{base}\n{renewal}")
        } else {
            base.to_string()
        }
    };
    let mut methods = vec![
        json!({ "methodIndex": 0, "type": "oauth", "label": text[0], "instructions": instruction(text[1]), "prompts": [], "browserMode": "external", "manualCallback": false }),
        json!({ "methodIndex": 1, "type": "api", "label": text[2], "instructions": instruction(text[3]), "prompts": [
            { "key": "accessToken", "type": "text", "label": "Access Token", "secret": true },
            { "key": "refreshToken", "type": "text", "label": "Refresh Token", "secret": true, "optional": true }
        ] }),
        json!({ "methodIndex": 2, "type": "api", "label": text[4], "instructions": instruction(text[6]), "prompts": [
            { "key": "credentialJson", "type": "text", "label": text[5], "secret": true }
        ] }),
        desktop,
    ];
    if !free {
        methods.push(json!({ "methodIndex": 4, "type": "api", "label": key_text[0], "instructions": key_text[1],
            "prompts": [{ "key": "apiKey", "type": "text", "label": key_text[2], "secret": true }] }));
    }
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
