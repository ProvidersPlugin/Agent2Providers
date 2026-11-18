use agent2api_wasm_sdk::Value;

pub fn login_methods(mut metadata: Value, locale: &str) -> Value {
    let labels = match locale {
        "en" => ["Browser sign-in", "Complete authorization on AtomGit, then return here and wait a few seconds.", "Enter tokens", "User ID", "The refresh token is optional. Automatic renewal is unavailable without it. If User ID is omitted, the official read-only profile API fills it in; chat is not sent until an identity is obtained.", "Import credentials JSON", "Credentials JSON", "Paste the AtomCode sign-in JSON, including access_token / refresh_token and user.id. Missing user identity is filled in through the read-only profile API."],
        "ja" => ["ブラウザーでログイン", "AtomGit の認証を完了し、ここに戻って数秒お待ちください。", "トークンを入力", "ユーザー ID", "Refresh Token は任意です。未入力の場合、自動更新はできません。ユーザー ID がない場合は公式の読み取り専用 API で補完し、確認できるまで会話を送信しません。", "認証情報 JSON を取り込む", "認証情報 JSON", "AtomCode のログイン結果の JSON を貼り付けてください。access_token / refresh_token と user.id に対応し、不足するユーザー情報は読み取り専用で補完します。"],
        "ko" => ["브라우저 로그인", "AtomGit에서 인증을 완료한 후 여기로 돌아와 잠시 기다리세요.", "토큰 직접 입력", "사용자 ID", "Refresh Token은 선택 사항입니다. 입력하지 않으면 자동 갱신할 수 없습니다. 사용자 ID가 없으면 공식 읽기 전용 API로 보완하며 확인 전에는 대화를 보내지 않습니다.", "인증 정보 JSON 가져오기", "인증 정보 JSON", "AtomCode 로그인 결과 JSON을 붙여넣으세요. access_token / refresh_token과 user.id를 지원하며 누락된 사용자 정보는 읽기 전용으로 보완합니다."],
        "pt-BR" => ["Entrar pelo navegador", "Conclua a autorização no AtomGit e volte aqui. Aguarde alguns segundos.", "Inserir tokens", "ID do usuário", "O Refresh Token é opcional. Sem ele, não há renovação automática. Se faltar o ID do usuário, ele será obtido pela API oficial somente de leitura; a conversa só será enviada após obter a identidade.", "Importar credenciais JSON", "Credenciais JSON", "Cole o JSON do login do AtomCode, incluindo access_token / refresh_token e user.id. Dados de identidade ausentes serão obtidos somente por leitura."],
        "zh-Hant" => ["瀏覽器登入", "在 AtomGit 授權頁完成登入；完成後回到這裡等待幾秒即可。", "手動填寫權杖", "使用者 ID", "Refresh Token 可選；未填寫時無法自動續期。使用者 ID 未填時會透過官方唯讀資料介面補全；未取得身分前不會傳送對話。", "匯入憑證 JSON", "憑證 JSON", "貼上 AtomCode 登入輸出的 JSON，支援 access_token / refresh_token 與 user.id 欄位；缺少使用者身分時會唯讀補全。"],
        _ => return metadata,
    };
    for (path, value) in [
        ("/methods/0/label", labels[0]), ("/methods/0/instructions", labels[1]),
        ("/methods/1/label", labels[2]), ("/methods/1/prompts/2/label", labels[3]),
        ("/methods/1/instructions", labels[4]), ("/methods/2/label", labels[5]),
        ("/methods/2/prompts/0/label", labels[6]), ("/methods/2/instructions", labels[7]),
    ] {
        if let Some(target) = metadata.pointer_mut(path) { *target = Value::String(value.to_string()); }
    }
    metadata
}
