use agent2api_wasm_sdk::{json, Value};

pub fn methods(locale: &str) -> Value {
    let labels = match locale {
        "en" => ["Browser sign-in", "Sign in on Huawei Cloud to add the account automatically; no callback URL needs to be pasted.", "Enter model credentials", "Import credentials JSON", "Credentials JSON", "Official model gateway URL", "Select or paste an OfficeAce credential file to import the account; automatic renewal is supported when complete sign-in details are included."],
        "zh-Hant" => ["網頁登入", "在華為雲完成授權即可自動新增帳號，無需貼上回呼網址。", "填寫模型憑證", "匯入憑證 JSON", "憑證 JSON", "官方模型網關位址", "選擇或貼上 OfficeAce 憑據檔案即可匯入帳號；包含完整登入資訊時支援自動續期。"],
        "ja" => ["ブラウザーでログイン", "Huawei Cloud で認証すると自動でアカウントを追加します。コールバック URL の貼り付けは不要です。", "モデル認証情報を入力", "認証 JSON を取り込む", "認証情報 JSON", "公式モデルゲートウェイ URL", "OfficeAce の認証ファイルを選択または貼り付けてアカウントを取り込めます。完全なログイン情報が含まれていれば自動更新に対応します。"],
        "ko" => ["브라우저 로그인", "Huawei Cloud에서 인증하면 계정이 자동 추가되며 콜백 URL을 붙여넣을 필요가 없습니다.", "모델 인증 정보 입력", "인증 JSON 가져오기", "인증 정보 JSON", "공식 모델 게이트웨이 주소", "OfficeAce 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있으며, 전체 로그인 정보가 포함되어 있으면 자동 갱신을 지원합니다."],
        "pt-BR" => ["Entrar pelo navegador", "Autorize na Huawei Cloud para adicionar a conta automaticamente; não é necessário colar a URL de callback.", "Inserir credenciais do modelo", "Importar JSON de credenciais", "JSON de credenciais", "URL oficial do gateway de modelos", "Selecione ou cole um arquivo de credenciais do OfficeAce para importar a conta; a renovação automática é suportada quando os dados completos de login estão incluídos."],
        _ => ["网页登录", "在华为云完成授权即可自动添加账号，无需粘贴回调地址。", "填写模型凭据", "导入凭据 JSON", "凭据 JSON", "官方模型网关地址", "选择或粘贴 OfficeAce 凭据文件即可导入账号；包含完整登录信息时支持自动续期。"],
    };
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": labels[0], "instructions": labels[1], "prompts": [],
            "browserMode": "choice", "manualCallback": false, "validatesCallback": true },
        { "methodIndex": 1, "type": "api", "label": labels[2], "prompts": [
            { "key": "baseUrl", "type": "text", "label": labels[5] },
            { "key": "modelAppKey", "type": "text", "label": "Model App Key", "secret": true },
            { "key": "modelAppSecret", "type": "text", "label": "Model App Secret", "secret": true }
        ] },
        { "methodIndex": 2, "type": "api", "label": labels[3], "instructions": labels[6], "prompts": [
            { "key": "credentialJson", "type": "text", "label": labels[4], "secret": true }
        ] }
    ] })
}
