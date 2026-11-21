//! 设备授权与手工文本入口，没有 PAT、本机票据读取或回调输入。
use agent2api_wasm_sdk::{json, Value};

pub fn instructions(locale: &str) -> &'static str {
    match locale {
        "en" => "Authorize your QwenWork China account on the official page; it is added automatically when the device poll completes. No local callback is used. When importing credentials, retain the original machine ID. A missing ID is generated once and saved; this fallback is not verified on a real account.",
        "ja" => "公式ページで千問辦公の中国版アカウントを認証すると、デバイス確認の完了後に自動追加されます。ローカルコールバックは不要です。認証情報の取り込みでは元のデバイス ID を保持してください。未指定の場合は一度生成して保存しますが、実アカウントでは未検証です。",
        "ko" => "공식 페이지에서 QwenWork 중국판 계정을 인증하면 기기 확인이 끝난 뒤 자동 추가됩니다. 로컬 콜백은 필요하지 않습니다. 인증 정보를 가져올 때 기존 기기 ID를 유지하세요. ID가 없으면 한 번 생성해 저장하며 실제 계정에서 검증되지는 않았습니다.",
        "pt-BR" => "Autorize sua conta QwenWork China na página oficial; a conta é adicionada automaticamente ao concluir a consulta do dispositivo, sem callback local. Preserve o ID original da máquina ao importar credenciais. Se ausente, ele é gerado uma vez e salvo; esse padrão não foi validado com uma conta real.",
        "zh-Hant" => "請在官方頁面授權千問辦公國內版帳號，裝置輪詢完成後會自動添加，不需要本機回呼。匯入憑證時請保留原裝置編號；缺少時只生成並保存一次，這條預設路徑尚未真實帳號驗收。",
        _ => "请在官方页面授权千问办公国内版账号，设备轮询完成后会自动添加，不需要本机回调。导入凭据时请保留原设备编号；缺少时只生成并保存一次，这条默认路径尚未真实账号验收。",
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
        "en" => ["Browser device authorization", "Authorize your QwenWork China account on the official page. No local callback is used. A missing machine ID uses a single persisted UUID, whose compatibility has not been verified.", "Enter device credentials", "Import credentials JSON", "Credentials JSON"],
        "ja" => ["ブラウザーでデバイス認証", "公式ページで千問辦公の中国版アカウントを認証します。ローカルコールバックは不要です。デバイス ID がない場合は一度だけ UUID を生成して保存しますが、その互換性は未検証です。", "デバイス認証情報を入力", "認証情報 JSON を取り込む", "認証情報 JSON"],
        "ko" => ["브라우저 기기 인증", "공식 페이지에서 QwenWork 중국판 계정을 인증합니다. 로컬 콜백은 사용하지 않습니다. 기기 ID가 없으면 UUID를 한 번 생성해 저장하며 이 기본값의 호환성은 검증되지 않았습니다.", "기기 인증 정보 입력", "인증 정보 JSON 가져오기", "인증 정보 JSON"],
        "pt-BR" => ["Autorizar dispositivo no navegador", "Autorize sua conta QwenWork China na página oficial, sem callback local. Na ausência de ID da máquina, um UUID é gerado uma única vez e salvo; a compatibilidade desse padrão não foi verificada.", "Inserir credenciais do dispositivo", "Importar credenciais JSON", "Credenciais JSON"],
        "zh-Hant" => ["網頁裝置授權", "請在官方頁面授權千問辦公國內版帳號，不需要本機回呼。缺少既有裝置編號時只生成並保存一次 UUID，該預設路徑相容性尚未實測。", "手動填寫裝置憑證", "匯入憑證 JSON", "憑證 JSON"],
        _ => ["网页设备授权", instructions(locale), "手工填写设备凭据", "导入凭据 JSON", "凭据 JSON"],
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
        { "methodIndex": 1, "type": "api", "label": text[2], "instructions": instructions(locale), "prompts": [
            { "key": "accessToken", "type": "text", "label": "Access Token", "secret": true },
            { "key": "refreshToken", "type": "text", "label": "Refresh Token", "secret": true, "optional": true },
            { "key": "userId", "type": "text", "label": id_labels[0], "optional": true },
            { "key": "expiresAt", "type": "text", "label": id_labels[1], "optional": true },
            { "key": "machineId", "type": "text", "label": id_labels[2], "optional": true }
        ] },
        { "methodIndex": 2, "type": "api", "label": text[3], "instructions": instructions(locale), "prompts": [
            { "key": "credentialJson", "type": "text", "label": text[4], "secret": true }
        ] }
    ] })
}
