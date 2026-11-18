use agent2api_wasm_sdk::{json, Value};

pub fn methods(locale: &str) -> Value {
    let labels = match locale {
        "en" => ["Browser sign-in", "Sign in to Huawei Cloud; credentials are returned by cloud polling. No local callback or extra permission setup is required. The account gateway currently supports direct connections only.", "Enter model credentials", "Import credentials JSON", "Credentials JSON", "Official model gateway URL"],
        "zh-Hant" => ["網頁登入", "在華為雲完成授權後自動輪詢取得憑證，無需本機回呼或額外權限設定。帳號閘道目前只支援直連。", "填寫模型憑證", "匯入憑證 JSON", "憑證 JSON", "官方模型網關位址"],
        "ja" => ["ブラウザーでログイン", "Huawei Cloud で認証するとクラウドのポーリングで取得します。ローカルのコールバックや追加の権限設定は不要です。アカウントのゲートウェイは現在、直接接続のみ対応しています。", "モデル認証情報を入力", "認証 JSON を取り込む", "認証情報 JSON", "公式モデルゲートウェイ URL"],
        "ko" => ["브라우저 로그인", "Huawei Cloud 인증 후 클라우드 조회로 인증 정보를 가져옵니다. 로컬 콜백이나 추가 권한 설정은 필요하지 않습니다. 계정 게이트웨이는 현재 직접 연결만 지원합니다.", "모델 인증 정보 입력", "인증 JSON 가져오기", "인증 정보 JSON", "공식 모델 게이트웨이 주소"],
        "pt-BR" => ["Entrar pelo navegador", "Autorize na Huawei Cloud. As credenciais chegam por consulta à nuvem, sem retorno local nem configuração extra de permissões. O gateway da conta aceita apenas conexão direta no momento.", "Inserir credenciais do modelo", "Importar JSON de credenciais", "JSON de credenciais", "URL oficial do gateway de modelos"],
        _ => ["网页登录", "在华为云完成授权后自动轮询取得凭据，无需本机回调或额外权限设置。账号网关目前只支持直连。", "填写模型凭据", "导入凭据 JSON", "凭据 JSON", "官方模型网关地址"],
    };
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": labels[0], "instructions": labels[1], "prompts": [],
            "browserMode": "external", "manualCallback": false, "validatesCallback": true },
        { "methodIndex": 1, "type": "api", "label": labels[2], "prompts": [
            { "key": "baseUrl", "type": "text", "label": labels[5] },
            { "key": "modelAppKey", "type": "text", "label": "Model App Key", "secret": true },
            { "key": "modelAppSecret", "type": "text", "label": "Model App Secret", "secret": true }
        ] },
        { "methodIndex": 2, "type": "api", "label": labels[3], "prompts": [
            { "key": "credentialJson", "type": "text", "label": labels[4], "secret": true }
        ] }
    ] })
}
