use agent2api_wasm_sdk::Value;

pub fn login_methods(mut metadata: Value, locale: &str) -> Value {
    let labels = match locale {
        "en" => ["Browser sign-in", "Complete authorization on AtomGit, then return here and wait a few seconds.", "Enter tokens", "User ID", "The refresh token is optional. Automatic renewal is unavailable without it.", "Import credentials JSON", "Credentials JSON", "Paste the JSON from AtomCode sign-in, including access_token and refresh_token."],
        "ja" => ["ブラウザーでログイン", "AtomGit の認証を完了し、ここに戻って数秒お待ちください。", "トークンを入力", "ユーザー ID", "Refresh Token は任意です。未入力の場合、自動更新はできません。", "認証情報 JSON を取り込む", "認証情報 JSON", "AtomCode のログイン結果の JSON を貼り付けてください。access_token / refresh_token に対応しています。"],
        "ko" => ["브라우저 로그인", "AtomGit에서 인증을 완료한 후 여기로 돌아와 잠시 기다리세요.", "토큰 직접 입력", "사용자 ID", "Refresh Token은 선택 사항입니다. 입력하지 않으면 자동 갱신할 수 없습니다.", "인증 정보 JSON 가져오기", "인증 정보 JSON", "AtomCode 로그인 결과의 JSON을 붙여넣으세요. access_token / refresh_token을 지원합니다."],
        "pt-BR" => ["Entrar pelo navegador", "Conclua a autorização no AtomGit e volte aqui. Aguarde alguns segundos.", "Inserir tokens", "ID do usuário", "O Refresh Token é opcional. Sem ele, não há renovação automática.", "Importar credenciais JSON", "Credenciais JSON", "Cole o JSON do login do AtomCode, incluindo access_token e refresh_token."],
        "zh-Hant" => ["瀏覽器登入", "在 AtomGit 授權頁完成登入；完成後回到這裡等待幾秒即可。", "手動填寫權杖", "使用者 ID", "Refresh Token 可選；未填寫時無法自動續期。", "匯入憑證 JSON", "憑證 JSON", "貼上 AtomCode 登入輸出的 JSON，支援 access_token / refresh_token 欄位。"],
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
