//! 国内短信走主机可信 SMS flow；本机导入是独立的无浏览器入口。
use agent2api_wasm_sdk::{json, login::sms, Value};

pub fn methods(locale: &str) -> Value {
    let (text, sms_hint, manual_hint, desktop_hint) = match locale {
        "en" => (
            ["SMS sign-in", "Mainland China phone number", "Enter credentials", "Import JSON", "Credentials JSON"],
            "Uses a mainland China phone number and a 6-digit SMS code. If official risk verification blocks the flow, sign in to the official client and import its local session; no unsupported challenge is bypassed.",
            "Imports a separate credential snapshot. An optional refresh token allows automatic renewal and may rotate only once; do not reuse the same refresh token concurrently in the official client. enc: values are decrypted only for the exact credentials you submit, using this gateway computer's fixed AutoClaw Local State file.",
            "Saves a live local reference. Renewal and writing rotated credentials back require separate permission in plugin settings; without it, sign in again in the official client. Select the edition matching the installed client.",
        ),
        "ja" => (
            ["SMS ログイン", "中国本土の電話番号", "認証情報を入力", "JSON を取り込む", "認証情報 JSON"],
            "中国本土の電話番号と6桁のSMSコードを使用します。公式のリスク認証で止まる場合は、公式クライアントでログインしてローカルセッションを取り込んでください。未対応の認証は回避しません。",
            "独立した認証情報のスナップショットを保存します。任意の更新トークンで自動更新できますが、一度だけ更新されるため公式クライアントと同時に使わないでください。enc: は今回入力した認証情報のみ、ゲートウェイのコンピューター上の固定された AutoClaw Local State で復号します。",
            "本機ログインの参照を保存します。更新と認証情報の書き戻しにはプラグイン設定で別途許可が必要です。許可しない場合は公式クライアントで再ログインしてください。インストールした版を選択してください。",
        ),
        "ko" => (
            ["SMS 로그인", "중국 본토 전화번호", "인증 정보 입력", "JSON 가져오기", "인증 정보 JSON"],
            "중국 본토 전화번호와 6자리 SMS 코드를 사용합니다. 공식 위험 검증으로 차단되면 공식 클라이언트에 로그인한 후 로컬 세션을 가져오세요. 지원하지 않는 검증을 우회하지 않습니다.",
            "별도의 인증 정보 스냅샷을 저장합니다. 선택적 갱신 토큰으로 자동 갱신할 수 있지만 한 번 사용 후 교체되므로 공식 클라이언트와 동시에 사용하지 마세요. enc:는 이번에 제출한 정확한 인증 정보만 게이트웨이 컴퓨터의 고정 AutoClaw Local State 파일로 복호화합니다.",
            "로컬 로그인 참조를 저장합니다. 갱신과 새 인증 정보 저장은 플러그인 설정에서 별도 허용해야 합니다. 허용하지 않으면 공식 클라이언트에 다시 로그인하세요. 설치한 클라이언트와 맞는 버전을 선택하세요.",
        ),
        "pt-BR" => (
            ["Entrar por SMS", "Telefone da China continental", "Inserir credenciais", "Importar JSON", "Credenciais JSON"],
            "Usa um telefone da China continental e um código SMS de 6 dígitos. Se a verificação oficial bloquear o processo, entre no cliente oficial e importe a sessão local; verificações não suportadas não são contornadas.",
            "Salva uma cópia independente das credenciais. O token opcional de renovação permite atualizar o login, mas pode ser rotacionado uma única vez; não use o mesmo token simultaneamente no cliente oficial. Valores enc: são descriptografados apenas para o par exato enviado, com o Local State fixo do AutoClaw no computador do gateway.",
            "Salva uma referência ao login local. Renovar e gravar novas credenciais exige permissão separada no plugin; sem ela, entre novamente no cliente oficial. Selecione a edição do cliente instalado.",
        ),
        "zh-Hant" => (
            ["手機驗證碼登入", "中國大陸手機號", "手動填寫憑證", "匯入 JSON", "憑證 JSON"],
            "使用中國大陸手機號與6位簡訊驗證碼。若官方風控阻斷，請在官方用戶端登入後匯入本機登入態；不繞過未接通的驗證。",
            "保存獨立憑證快照。選填刷新令牌可自動續期，但令牌可能一次使用後輪換，請勿與官方用戶端同時使用同一份刷新令牌。enc: 僅用所連接閘道電腦的 AutoClaw 固定 Local State 解密本次明確輸入的那一對憑證。",
            "保存本機即時引用。續期及將新憑據寫回原檔案需在插件設定中另外授權；未授權時請在官方用戶端重新登入。請選擇與實際安裝用戶端一致的版本。",
        ),
        _ => (
            ["手机验证码登录", "中国大陆手机号", "手动填写凭据", "导入 JSON", "凭据 JSON"],
            "使用中国大陆手机号与6位短信验证码。若官方风控阻断，请在官方客户端登录后导入本机登录态；不绕过未接通的验证。",
            "保存独立凭据快照。选填刷新令牌可自动续期，但令牌可能一次使用后轮换，请勿与官方客户端同时使用同一份刷新令牌。enc: 仅用所连接网关电脑的 AutoClaw 固定 Local State 解密本次明确输入的那一对凭据。",
            "保存本机实时引用。续期及将新凭据写回原文件需在插件设置中另外授权；未授权时请在官方客户端重新登录。请选择与实际安装客户端一致的版本。",
        ),
    };
    let mut sms_method = sms::method(
        0,
        text[0],
        sms::Config {
            phone_label: text[1].into(),
            phone_placeholder: "13800138000".into(),
            country_codes: vec![sms::CountryCode {
                value: "86".into(),
                label: "+86".into(),
            }],
            phone_min_length: 11,
            phone_max_length: 11,
            code_min_length: 6,
            code_max_length: 6,
            resend_after_seconds: 60,
            expires_after_seconds: 300,
        },
    );
    sms_method["instructions"] = json!(sms_hint);
    let mut desktop = agent2api_wasm_sdk::login::desktop_method(3, "AutoClaw", locale);
    let common = desktop["instructions"].as_str().unwrap_or("");
    desktop["instructions"] = json!(format!("{common} {desktop_hint}"));
    json!({ "methods": [
        sms_method,
        { "methodIndex": 1, "type": "api", "label": text[2], "instructions": manual_hint, "prompts": [
            { "key": "token", "type": "text", "label": "Access Token", "secret": true },
            { "key": "refreshToken", "type": "text", "label": "Refresh Token", "secret": true, "optional": true },
            { "key": "deviceId", "type": "text", "label": "Device ID", "optional": true }
        ] },
        { "methodIndex": 2, "type": "api", "label": text[3], "instructions": manual_hint, "prompts": [
            { "key": "credentialJson", "type": "text", "label": text[4], "secret": true }
        ] },
        desktop,
        agent2api_wasm_sdk::legacy_credentials::method(4, locale)
    ] })
}
