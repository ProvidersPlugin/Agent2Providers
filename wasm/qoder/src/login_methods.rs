//! 当前动态表单只登记 Qoder 中国版；不复用千问/国际身份，无桌面凭据导入或回调输入。
use agent2api_wasm_sdk::{json, Value};

pub fn instructions(locale: &str) -> &'static str {
    match locale {
        "en" => "Complete device authorization on the official Qoder China page. This login will add the account automatically; no callback address is needed.",
        "ja" => "Qoder 中国版の公式ページでデバイス認証を完了してください。完了後、アカウントが自動追加されます。コールバックアドレスは不要です。",
        "ko" => "Qoder 중국판 공식 페이지에서 기기 인증을 완료하세요. 계정이 자동으로 추가되며 콜백 주소는 필요하지 않습니다.",
        "pt-BR" => "Conclua a autorização do dispositivo na página oficial do Qoder China. A conta será adicionada automaticamente, sem endereço de callback.",
        "zh-Hant" => "請在 Qoder 中國版官方頁面完成裝置授權，帳號會自動新增，不需要貼上回呼位址。",
        _ => "请在 Qoder 中国版官方页面完成设备授权，账号会自动添加，不需要粘贴回调地址。",
    }
}

pub fn methods(locale: &str) -> Value {
    let text = match locale {
        "en" => ["Browser device authorization", "Authorize your Qoder China account on the official page to add the account automatically.", "Personal access token (PAT)", "Enter credentials", "Import credentials JSON", "Credentials JSON", "Select or paste a Qoder China credential file to import the account; automatic renewal is supported when complete sign-in details are included."],
        "ja" => ["ブラウザーでデバイス認証", "公式ページで Qoder 中国版を認証すると自動でアカウントが追加されます。", "個人アクセストークン（PAT）", "認証情報を入力", "認証情報 JSON を取り込む", "認証情報 JSON", "Qoder 中国版の認証ファイルを選択または貼り付けてアカウントを取り込めます。完全なログイン情報が含まれていれば自動更新に対応します。"],
        "ko" => ["브라우저 기기 인증", "공식 페이지에서 Qoder 중국판을 인증하면 계정이 자동으로 추가됩니다.", "개인 액세스 토큰 (PAT)", "인증 정보 입력", "인증 정보 JSON 가져오기", "인증 정보 JSON", "Qoder 중국판 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있으며, 전체 로그인 정보가 포함되어 있으면 자동 갱신을 지원합니다."],
        "pt-BR" => ["Autorizar dispositivo no navegador", "Autorize sua conta Qoder China na página oficial para adicionar a conta automaticamente.", "Token de acesso pessoal (PAT)", "Inserir credenciais", "Importar credenciais JSON", "Credenciais JSON", "Selecione ou cole um arquivo de credenciais do Qoder China para importar a conta; a renovação automática é suportada quando os dados completos de login estão incluídos."],
        "zh-Hant" => ["網頁裝置授權", "在官方頁面授權 Qoder 中國版帳號即可自動新增帳號。", "個人存取權杖（PAT）", "手動填寫憑證", "匯入憑證 JSON", "憑證 JSON", "選擇或貼上 Qoder 中國版憑據檔案即可匯入帳號；包含完整登入資訊時支援自動續期。"],
        _ => ["网页设备授权", "在官方页面授权 Qoder 中国版账号即可自动添加账号。", "个人访问令牌（PAT）", "手动填写凭据", "导入凭据 JSON", "凭据 JSON", "选择或粘贴 Qoder 中国版凭据文件即可导入账号；包含完整登录信息时支持自动续期。"],
    };
    let fields = match locale {
        "en" => [
            "Access token",
            "Refresh token",
            "User ID",
            "Device ID",
            "Expires at (timestamp or ISO 8601)",
        ],
        "ja" => [
            "アクセストークン",
            "更新トークン",
            "ユーザー ID",
            "デバイス ID",
            "有効期限（タイムスタンプまたは ISO 8601）",
        ],
        "ko" => [
            "액세스 토큰",
            "갱신 토큰",
            "사용자 ID",
            "기기 ID",
            "만료 시각 (타임스탬프 또는 ISO 8601)",
        ],
        "pt-BR" => [
            "Token de acesso",
            "Token de renovação",
            "ID do usuário",
            "ID do dispositivo",
            "Expira em (timestamp ou ISO 8601)",
        ],
        "zh-Hant" => [
            "存取權杖",
            "更新權杖",
            "使用者 ID",
            "裝置 ID",
            "到期時間（時間戳記或 ISO 8601）",
        ],
        _ => [
            "访问令牌",
            "刷新令牌",
            "用户 ID",
            "设备 ID",
            "到期时间（时间戳或 ISO 8601）",
        ],
    };
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": text[0], "instructions": text[1], "prompts": [],
            "browserMode": "choice", "manualCallback": false, "callbackInput": "url" },
        { "methodIndex": 1, "type": "api", "label": text[2], "prompts": [
            { "key": "pat", "type": "text", "label": "PAT", "secret": true }
        ] },
        { "methodIndex": 2, "type": "api", "label": text[3], "prompts": [
            { "key": "accessToken", "type": "text", "label": fields[0], "secret": true },
            { "key": "refreshToken", "type": "text", "label": fields[1], "secret": true, "optional": true },
            { "key": "userId", "type": "text", "label": fields[2], "optional": true },
            { "key": "machineId", "type": "text", "label": fields[3], "optional": true },
            { "key": "expiresAt", "type": "text", "label": fields[4], "optional": true }
        ] },
        { "methodIndex": 3, "type": "api", "label": text[4], "instructions": text[6], "prompts": [
            { "key": "credentialJson", "type": "text", "label": text[5], "secret": true }
        ] }
    ] })
}
