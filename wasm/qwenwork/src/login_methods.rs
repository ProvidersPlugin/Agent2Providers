//! 设备授权与手工文本入口，没有 PAT、本机票据读取或回调输入。
use agent2api_wasm_sdk::{json, Value};

pub fn instructions(locale: &str) -> &'static str {
    match locale {
        "en" => "Authorize your QwenWork China account on the official page to add the account automatically.",
        "ja" => "公式ページで千問辦公の中国版アカウントを認証すると自動で追加されます。",
        "ko" => "공식 페이지에서 QwenWork 중국판 계정을 인증하면 자동으로 추가됩니다.",
        "pt-BR" => "Autorize sua conta QwenWork China na página oficial para adicionar a conta automaticamente.",
        "zh-Hant" => "在官方頁面授權千問辦公國內版帳號即可自動新增帳號。",
        _ => "在官方页面授权千问办公国内版账号即可自动添加账号。",
    }
}

pub fn progress(locale: &str, step: &str) -> &'static str {
    match (locale, step) {
        ("en", "expired") => "Device authorization was canceled or expired; start again.",
        ("ja", "expired") => "デバイス認証が取り消されたか期限切れです。再度開始してください。",
        ("ko", "expired") => "기기 인증이 취소되었거나 만료되었습니다. 다시 시작하세요.",
        ("pt-BR", "expired") => "A autorização foi cancelada ou expirou; inicie novamente.",
        ("zh-Hant", "expired") => "裝置授權已取消或過期，請重新發起。",
        (_, "expired") => "设备授权已取消或过期，请重新发起。",
        ("en", "unknown") => "The previous authorization result is unknown. Start again; the credential request was not repeated.",
        ("ja", "unknown") => "前回の認証結果を確認できません。認証情報を再要求せず、新しい認証を開始してください。",
        ("ko", "unknown") => "이전 인증 결과를 확인하지 못했습니다. 인증 정보를 다시 요청하지 않았으니 새 인증을 시작하세요.",
        ("pt-BR", "unknown") => "O resultado anterior não foi confirmado. Inicie novamente; as credenciais não foram solicitadas outra vez.",
        ("zh-Hant", "unknown") => "上輪授權結果未確認，請重新發起；未重複領取裝置憑證。",
        (_, "unknown") => "上轮授权结果未确认，请重新发起；未重复领取设备凭据。",
        ("en", _) => "The authorization service is temporarily unreachable. Waiting before retrying.",
        ("ja", _) => "認証サービスに一時的に接続できません。再試行を待っています。",
        ("ko", _) => "인증 서비스에 일시적으로 연결할 수 없습니다. 재시도를 기다립니다.",
        ("pt-BR", _) => "O serviço de autorização está temporariamente indisponível. Aguardando uma nova tentativa.",
        ("zh-Hant", _) => "裝置授權服務暫時不可達，正在等待重試。",
        _ => "设备授权服务暂时不可达，正在等待重试。",
    }
}

pub fn methods(locale: &str) -> Value {
    let text = match locale {
        "en" => ["Browser device authorization", "Enter an Access Token to add the account; include a Refresh Token to enable automatic renewal.", "Enter device credentials", "Import credentials JSON", "Credentials JSON", "Select or paste a QwenWork China credential file to import the account; automatic renewal is supported when complete sign-in details are included."],
        "ja" => ["ブラウザーでデバイス認証", "Access Token を入力してアカウントを追加できます。Refresh Token を入力すると自動更新に対応します。", "デバイス認証情報を入力", "認証情報 JSON を取り込む", "認証情報 JSON", "千問辦公（中国版）の認証ファイルを選択または貼り付けてアカウントを取り込めます。完全なログイン情報が含まれていれば自動更新に対応します。"],
        "ko" => ["브라우저 기기 인증", "Access Token을 입력해 계정을 추가할 수 있으며, Refresh Token을 함께 입력하면 자동 갱신을 지원합니다.", "기기 인증 정보 입력", "인증 정보 JSON 가져오기", "인증 정보 JSON", "QwenWork 중국판 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있으며, 전체 로그인 정보가 포함되어 있으면 자동 갱신을 지원합니다."],
        "pt-BR" => ["Autorizar dispositivo no navegador", "Informe o Access Token para adicionar a conta; inclua o Refresh Token para ativar a renovação automática.", "Inserir credenciais do dispositivo", "Importar credenciais JSON", "Credenciais JSON", "Selecione ou cole um arquivo de credenciais do QwenWork China para importar a conta; a renovação automática é suportada quando os dados completos de login estão incluídos."],
        "zh-Hant" => ["網頁裝置授權", "填寫 Access Token 即可新增帳號；填寫 Refresh Token 後支援自動續期。", "手動填寫裝置憑證", "匯入憑證 JSON", "憑證 JSON", "選擇或貼上千問辦公國內版憑據檔案即可匯入帳號；包含完整登入資訊時支援自動續期。"],
        _ => ["网页设备授权", "填写 Access Token 即可添加账号；填写 Refresh Token 后支持自动续期。", "手工填写设备凭据", "导入凭据 JSON", "凭据 JSON", "选择或粘贴千问办公国内版凭据文件即可导入账号；包含完整登录信息时支持自动续期。"],
    };
    let id_labels = match locale {
        "en" => ["User ID", "Expires at", "Original machine ID"],
        "ja" => ["ユーザー ID", "有効期限", "元のデバイス ID"],
        "ko" => ["사용자 ID", "만료 시각", "기존 기기 ID"],
        "pt-BR" => ["ID do usuário", "Expira em", "ID original da máquina"],
        "zh-Hant" => ["使用者編號", "到期時間", "原裝置編號"],
        _ => ["用户编号", "到期时间", "原设备编号"],
    };
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": text[0], "instructions": instructions(locale), "prompts": [],
            "browserMode": "choice", "manualCallback": false, "callbackInput": "url" },
        { "methodIndex": 1, "type": "api", "label": text[2], "instructions": text[1], "prompts": [
            { "key": "accessToken", "type": "text", "label": "Access Token", "secret": true },
            { "key": "refreshToken", "type": "text", "label": "Refresh Token", "secret": true, "optional": true },
            { "key": "userId", "type": "text", "label": id_labels[0], "optional": true },
            { "key": "expiresAt", "type": "text", "label": id_labels[1], "optional": true },
            { "key": "machineId", "type": "text", "label": id_labels[2], "optional": true }
        ] },
        { "methodIndex": 2, "type": "api", "label": text[3], "instructions": text[5], "prompts": [
            { "key": "credentialJson", "type": "text", "label": text[4], "secret": true }
        ] }
    ] })
}
