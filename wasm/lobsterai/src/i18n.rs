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
        "en" => ["Browser sign-in", "Complete Youdao sign-in to add the account; when accessing the panel remotely, paste the callback URL after signing in.",
            "Import credentials JSON", "Credentials JSON", "Select or paste a LobsterAI credential file to import the account; automatic renewal is supported when complete sign-in details are included.",
            "Enter tokens", "User ID", "Enter an Access Token to add the account; include Refresh Token and Keyfrom fields to enable automatic renewal."],
        "ja" => ["ブラウザーでログイン", "有道でログインするとアカウントを追加できます。リモート画面ではログイン後にコールバック URL を貼り付けてください。",
            "認証情報 JSON を取り込む", "認証情報 JSON", "LobsterAI の認証ファイルを選択または貼り付けてアカウントを取り込めます。完全なログイン情報が含まれていれば自動更新に対応します。",
            "トークンを入力", "ユーザー ID", "Access Token を入力してアカウントを追加できます。Refresh Token と Keyfrom を入力すると自動更新に対応します。"],
        "ko" => ["브라우저 로그인", "Youdao에서 로그인하면 계정이 추가됩니다. 원격 패널에서는 로그인 후 콜백 URL을 붙여넣으세요.",
            "인증 정보 JSON 가져오기", "인증 정보 JSON", "LobsterAI 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있으며, 전체 로그인 정보가 포함되어 있으면 자동 갱신을 지원합니다.",
            "토큰 직접 입력", "사용자 ID", "Access Token을 입력해 계정을 추가할 수 있으며, Refresh Token과 Keyfrom을 함께 입력하면 자동 갱신을 지원합니다."],
        "pt-BR" => ["Entrar pelo navegador", "Conclua o login no Youdao para adicionar a conta; no acesso remoto ao painel, cole a URL de callback após o login.",
            "Importar credenciais JSON", "Credenciais JSON", "Selecione ou cole um arquivo de credenciais do LobsterAI para importar a conta; a renovação automática é suportada quando os dados completos de login estão incluídos.",
            "Inserir tokens", "ID do usuário", "Informe o Access Token para adicionar a conta; inclua Refresh Token e Keyfrom para ativar a renovação automática."],
        "zh-Hant" => ["瀏覽器登入", "在有道授權頁完成登入即可新增帳號；遠端存取面板時，按提示貼上登入後的回呼網址即可。",
            "匯入憑證 JSON", "憑證 JSON", "選擇或貼上 LobsterAI 憑據檔案即可匯入帳號；包含完整登入資訊時支援自動續期。",
            "手動填寫權杖", "使用者 ID", "填寫 Access Token 即可新增帳號；一併填寫 Refresh Token 與 Keyfrom 時支援自動續期。"],
        _ => ["浏览器登录", "在有道授权页完成登录即可添加账号；远程访问面板时，按提示粘贴登录后的回调地址即可。",
            "导入凭证 JSON", "凭证 JSON", "选择或粘贴 LobsterAI 凭据文件即可导入账号；包含完整登录信息时支持自动续期。",
            "手动填写令牌", "用户 ID", "填写 Access Token 即可添加账号；一并填写 Refresh Token 与 Keyfrom 时支持自动续期。"],
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
