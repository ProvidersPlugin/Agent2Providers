use agent2api_wasm_sdk::Value;

pub fn login_methods(mut metadata: Value, locale: &str) -> Value {
    let labels = match locale {
        "en" => ["Browser sign-in", "Complete authorization on AtomGit, then return here and wait a few seconds.", "Enter tokens", "User ID", "Enter an Access Token to add the account; include a Refresh Token to enable automatic renewal.", "Import credentials JSON", "Credentials JSON", "Select or paste an AtomCode credential file to import the account; automatic renewal is supported when complete sign-in details are included."],
        "ja" => ["ブラウザーでログイン", "AtomGit の認証を完了し、ここに戻って数秒お待ちください。", "トークンを入力", "ユーザー ID", "Access Token を入力してアカウントを追加できます。Refresh Token を入力すると自動更新に対応します。", "認証情報 JSON を取り込む", "認証情報 JSON", "AtomCode の認証ファイルを選択または貼り付けてアカウントを取り込めます。完全なログイン情報が含まれていれば自動更新に対応します。"],
        "ko" => ["브라우저 로그인", "AtomGit에서 인증을 완료한 후 여기로 돌아와 잠시 기다리세요.", "토큰 직접 입력", "사용자 ID", "Access Token을 입력해 계정을 추가할 수 있으며, Refresh Token을 함께 입력하면 자동 갱신을 지원합니다.", "인증 정보 JSON 가져오기", "인증 정보 JSON", "AtomCode 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있으며, 전체 로그인 정보가 포함되어 있으면 자동 갱신을 지원합니다."],
        "pt-BR" => ["Entrar pelo navegador", "Conclua a autorização no AtomGit e volte aqui. Aguarde alguns segundos.", "Inserir tokens", "ID do usuário", "Informe o Access Token para adicionar a conta; inclua o Refresh Token para ativar a renovação automática.", "Importar credenciais JSON", "Credenciais JSON", "Selecione ou cole um arquivo de credenciais do AtomCode para importar a conta; a renovação automática é suportada quando os dados completos de login estão incluídos."],
        "zh-Hant" => ["瀏覽器登入", "在 AtomGit 授權頁完成登入；完成後回到這裡等待幾秒即可。", "手動填寫權杖", "使用者 ID", "填寫 Access Token 即可新增帳號；填寫 Refresh Token 後支援自動續期。", "匯入憑證 JSON", "憑證 JSON", "選擇或貼上 AtomCode 憑據檔案即可匯入帳號；包含完整登入資訊時支援自動續期。"],
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
