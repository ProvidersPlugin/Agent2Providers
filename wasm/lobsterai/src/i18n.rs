use agent2api_wasm_sdk::{json, Value};

pub fn locale(value: &str) -> &str {
    if matches!(value, "en" | "ja" | "ko" | "pt-BR" | "zh-Hant") {
        value
    } else {
        "zh-CN"
    }
}

pub fn login_text(language: &str, phase: &str) -> &'static str {
    let messages = match locale(language) {
        "en" => ["Complete Youdao sign-in. For a remote gateway, paste the browser's full callback URL.",
            "Waiting for the Youdao callback; remote gateways also accept the full callback URL.",
            "The callback is being saved. Please wait; do not submit it again.",
            "Sign-in ended or expired. Please start again.", "Authorization was declined or canceled. Please start again."],
        "ja" => ["有道でログインしてください。リモート接続では最後の完全なコールバック URL を貼り付けます。",
            "有道のコールバックを待っています。リモート接続では完全な URL を貼り付けられます。",
            "認証結果を保存しています。再送せずにお待ちください。",
            "ログインが終了または期限切れになりました。再度開始してください。", "認証が拒否またはキャンセルされました。再度開始してください。"],
        "ko" => ["Youdao에서 로그인하세요. 원격 게이트웨이에서는 브라우저의 전체 콜백 URL을 붙여넣으세요.",
            "Youdao 콜백을 기다립니다. 원격 게이트웨이에는 전체 콜백 URL을 붙여넣을 수 있습니다.",
            "인증 결과를 저장하고 있습니다. 다시 제출하지 말고 기다리세요.",
            "로그인이 종료되거나 만료되었습니다. 다시 시작하세요.", "인증이 거부되거나 취소되었습니다. 다시 시작하세요."],
        "pt-BR" => ["Conclua o login no Youdao. Em um gateway remoto, cole a URL completa de retorno do navegador.",
            "Aguardando o retorno do Youdao; gateways remotos também aceitam a URL completa.",
            "O resultado do login está sendo salvo. Aguarde sem enviar novamente.",
            "O login terminou ou expirou. Inicie novamente.", "A autorização foi recusada ou cancelada. Inicie novamente."],
        "zh-Hant" => ["在有道授權頁完成登入；遠端網關請貼上瀏覽器最後停留的完整回呼網址。",
            "等待有道授權回呼；遠端網關可貼上完整回呼網址。", "正在儲存授權結果，請等待，不要重複提交。",
            "本次登入已結束或過期，請重新發起。", "有道授權已拒絕或取消，請重新發起登入。"],
        _ => ["在有道授权页完成登录；远程网关请粘贴浏览器最后停留的完整回调地址。",
            "等待有道授权回调；远程网关可粘贴完整回调地址。", "正在保存授权结果，请等待，不要重复提交。",
            "本次登录已结束或过期，请重新发起。", "有道授权已拒绝或取消，请重新发起登录。"],
    };
    messages[match phase {
        "start" => 0,
        "waiting" => 1,
        "processing" => 2,
        "denied" => 4,
        _ => 3,
    }]
}

pub fn methods(locale: &str) -> Value {
    let labels = match locale {
        "en" => ["Browser sign-in", "Complete Youdao sign-in. On a remote server, paste the full callback URL from the browser.",
            "Import credentials JSON", "Credentials JSON", "Paste saved LobsterAI credentials. Renewal needs the refresh token and both original keyfrom values; an access token alone cannot renew.",
            "Enter tokens", "User ID", "The first/latest keyfrom values must come from the original sign-in. Missing refresh fields means sign-in is required after expiry."],
        "ja" => ["ブラウザーでログイン", "有道でログインしてください。リモート環境ではブラウザーの完全なコールバック URL を貼り付けます。",
            "認証情報 JSON を取り込む", "認証情報 JSON", "保存した LobsterAI 認証情報を貼り付けてください。更新には更新トークンと元の両 keyfrom が必要です。アクセストークンのみでは更新できません。",
            "トークンを入力", "ユーザー ID", "first/latest keyfrom は最初のログイン時の値を使用してください。更新情報がない場合は期限後に再ログインが必要です。"],
        "ko" => ["브라우저 로그인", "Youdao에서 로그인하세요. 원격 서버에서는 브라우저의 전체 콜백 URL을 붙여넣으세요.",
            "인증 정보 JSON 가져오기", "인증 정보 JSON", "저장한 LobsterAI 인증 정보를 붙여넣으세요. 갱신에는 갱신 토큰과 원래 keyfrom 값 두 개가 필요합니다. 액세스 토큰만으로는 갱신할 수 없습니다.",
            "토큰 직접 입력", "사용자 ID", "first/latest keyfrom은 최초 로그인 값을 사용해야 합니다. 갱신 정보가 없으면 만료 후 다시 로그인해야 합니다."],
        "pt-BR" => ["Entrar pelo navegador", "Conclua o login no Youdao. Em servidor remoto, cole a URL completa de retorno do navegador.",
            "Importar credenciais JSON", "Credenciais JSON", "Cole as credenciais salvas do LobsterAI. A renovação exige o refresh token e os dois valores keyfrom originais; só o access token não permite renovar.",
            "Inserir tokens", "ID do usuário", "Use os valores first/latest keyfrom do login original. Sem os dados de renovação, será necessário entrar novamente após expirar."],
        "zh-Hant" => ["瀏覽器登入", "在有道授權頁完成登入；遠端部署請貼上瀏覽器最後停留的完整回呼網址。",
            "匯入憑證 JSON", "憑證 JSON", "貼上已儲存的 LobsterAI 憑證。自動續期需要 refresh token 和原始兩個 keyfrom；只有 access token 時無法續期。",
            "手動填寫權杖", "使用者 ID", "first/latest keyfrom 必須是首次登入時的原值；缺少續期欄位時，過期後需重新登入。"],
        _ => ["浏览器登录", "在有道授权页完成登录；远程部署请粘贴浏览器最后停留的完整回调地址。",
            "导入凭证 JSON", "凭证 JSON", "粘贴已保存的 LobsterAI 凭证。自动续期需要 refresh token 和原始两个 keyfrom；只有 access token 时无法续期。",
            "手动填写令牌", "用户 ID", "first/latest keyfrom 必须是首次登录时的原值；缺少续期字段时，过期后需重新登录。"],
    };
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": labels[0], "prompts": [],
          "browserMode": "choice", "manualCallback": true, "callbackInput": "url", "validatesCallback": true,
          "instructions": labels[1] },
        { "methodIndex": 1, "type": "api", "label": labels[2], "prompts": [
            { "key": "credentialJson", "type": "text", "label": labels[3], "secret": true }
          ], "instructions": labels[4] },
        { "methodIndex": 2, "type": "api", "label": labels[5], "prompts": [
            { "key": "accessToken", "type": "text", "label": "Access Token", "secret": true },
            { "key": "refreshToken", "type": "text", "label": "Refresh Token", "secret": true, "optional": true },
            { "key": "userId", "type": "text", "label": labels[6], "optional": true },
            { "key": "uuid", "type": "text", "label": "UUID", "secret": true, "optional": true },
            { "key": "firstKeyfrom", "type": "text", "label": "First Keyfrom", "secret": true, "optional": true },
            { "key": "latestKeyfrom", "type": "text", "label": "Latest Keyfrom", "secret": true, "optional": true }
          ], "instructions": labels[7] }
    ] })
}
