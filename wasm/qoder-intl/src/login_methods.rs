//! 表单只登记 Qoder 国际版；无国内、千问、桌面凭据导入或回调输入。
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
pub fn notice(locale: &str, phase: &str) -> &'static str {
    let rows = match locale {
        "en" => ["Complete device authorization on the official Qoder International page. The account will be added automatically.", "Device authorization expired or was canceled. Start again.", "The previous authorization result is unconfirmed. Start again; credentials were not requested twice.", "Device authorization expired. The late account result was not saved.", "Qoder's device authorization service is temporarily unreachable. Waiting to retry.", "Qoder device authorization failed. Check the account and connection, then start again."],
        "ja" => ["Qoder 国際版の公式ページでデバイス認証を完了してください。アカウントは自動追加されます。", "デバイス認証が期限切れまたはキャンセルされました。やり直してください。", "前の認証結果は未確認です。やり直してください。認証情報は重複取得していません。", "デバイス認証が期限切れです。遅れて届いたアカウントは保存していません。", "Qoder のデバイス認証サービスに一時的に接続できません。再試行を待っています。", "Qoder のデバイス認証に失敗しました。アカウントと接続を確認してやり直してください。"],
        "ko" => ["Qoder 국제판 공식 페이지에서 기기 인증을 완료하세요. 계정이 자동으로 추가됩니다.", "기기 인증이 만료되거나 취소되었습니다. 다시 시작하세요.", "이전 인증 결과를 확인하지 못했습니다. 다시 시작하세요. 인증 정보를 중복 요청하지 않았습니다.", "기기 인증이 만료되어 늦게 도착한 계정 결과를 저장하지 않았습니다.", "Qoder 기기 인증 서비스에 일시적으로 연결할 수 없습니다. 재시도를 기다립니다.", "Qoder 기기 인증에 실패했습니다. 계정과 연결을 확인하고 다시 시작하세요."],
        "pt-BR" => ["Conclua a autorização do dispositivo na página oficial do Qoder International. A conta será adicionada automaticamente.", "A autorização expirou ou foi cancelada. Inicie novamente.", "O resultado anterior não foi confirmado. Inicie novamente; as credenciais não foram solicitadas duas vezes.", "A autorização expirou. O resultado atrasado da conta não foi salvo.", "O serviço de autorização do Qoder está temporariamente indisponível. Aguardando uma nova tentativa.", "A autorização do dispositivo falhou. Confira a conta e a conexão e inicie novamente."],
        "zh-Hant" => ["請在 Qoder 國際版官方頁面完成裝置授權，帳號會自動新增。", "裝置授權已取消或過期，請重新發起。", "上輪授權結果未確認，請重新發起；未重複取得裝置憑證。", "裝置授權已過期，未儲存遲到的帳號結果。", "Qoder 裝置授權服務暫時無法連線，正在等待重試。", "Qoder 裝置授權未完成，請確認帳號與連線後重新發起。"],
        _ => ["请在 Qoder 国际版官方页面完成设备授权，账号会自动添加。", "设备授权已取消或过期，请重新发起。", "上轮授权结果未确认，请重新发起；未重复取得设备凭据。", "设备授权已过期，未保存迟到的账号结果。", "Qoder 设备授权服务暂时不可达，正在等待重试。", "Qoder 设备授权未完成，请确认账号与连接后重新发起。"],
    };
    rows[match phase {
        "start" => 0,
        "expired" => 1,
        "unconfirmed" => 2,
        "late" => 3,
        "retry" => 4,
        _ => 5,
    }]
}

pub fn methods(locale: &str) -> Value {
    let text = match locale {
        "en" => ["Browser device authorization", "Authorize your Qoder International account on the official page. PKCE is polled automatically; no local callback, client installation or UMID is required for login.", "Personal access token (PAT)", "Enter credentials", "Import credentials JSON", "Credentials JSON"],
        "ja" => ["ブラウザーでデバイス認証", "公式ページで Qoder 国際版を認証してください。PKCE の結果は自動取得され、ログインにローカルコールバックやクライアント、UMID は不要です。", "個人アクセストークン（PAT）", "認証情報を入力", "認証情報 JSON を取り込む", "認証情報 JSON"],
        "ko" => ["브라우저 기기 인증", "공식 페이지에서 Qoder 국제판을 인증해 주세요. PKCE 결과를 자동으로 가져오며 로그인에는 로컬 콜백, 클라이언트 설치 또는 UMID가 필요하지 않습니다.", "개인 액세스 토큰 (PAT)", "인증 정보 입력", "인증 정보 JSON 가져오기", "인증 정보 JSON"],
        "pt-BR" => ["Autorizar dispositivo no navegador", "Autorize sua conta Qoder International na página oficial. O resultado PKCE é consultado automaticamente; o login não exige callback local, cliente instalado ou UMID.", "Token de acesso pessoal (PAT)", "Inserir credenciais", "Importar credenciais JSON", "Credenciais JSON"],
        "zh-Hant" => ["網頁裝置授權", "請在官方頁面授權 Qoder 國際版帳號。PKCE 結果會自動輪詢，登入不需要本機回呼、安裝客戶端或 UMID。", "個人存取權杖（PAT）", "手動填寫憑證", "匯入憑證 JSON", "憑證 JSON"],
        _ => ["网页设备授权", "请在官方页面授权 Qoder 国际版账号。PKCE 结果会自动轮询，登录不需要本机回调、安装客户端或 UMID。", "个人访问令牌（PAT）", "手动填写凭据", "导入凭据 JSON", "凭据 JSON"],
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
        { "methodIndex": 3, "type": "api", "label": text[4], "prompts": [
            { "key": "credentialJson", "type": "text", "label": text[5], "secret": true }
        ] }
    ] })
}
