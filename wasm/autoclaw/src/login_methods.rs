//! 国内短信走主机可信 SMS flow；本机导入是独立的无浏览器入口。
use agent2api_wasm_sdk::{json, login::sms, Value};

pub fn methods(locale: &str) -> Value {
    let (text, sms_hint, manual_hint, file_hint) = match locale {
        "en" => (
            ["SMS sign-in", "Mainland China phone number", "Enter credentials", "Import JSON", "Credentials JSON"],
            "Enter a mainland China phone number and the 6-digit SMS code to sign in.",
            "Enter an Access Token to add the account; include a Refresh Token to enable automatic renewal.",
            "Select or paste an AutoClaw credential file to import the account; automatic renewal is supported when complete sign-in details are included.",
        ),
        "ja" => (
            ["SMS ログイン", "中国本土の電話番号", "認証情報を入力", "JSON を取り込む", "認証情報 JSON"],
            "中国本土の携帯電話番号と6桁のSMS認証コードでログインします。",
            "Access Token を入力してアカウントを追加できます。Refresh Token を入力すると自動更新に対応します。",
            "AutoClaw の認証ファイルを選択または貼り付けてアカウントを取り込めます。完全なログイン情報が含まれていれば自動更新に対応します。",
        ),
        "ko" => (
            ["SMS 로그인", "중국 본토 전화번호", "인증 정보 입력", "JSON 가져오기", "인증 정보 JSON"],
            "중국 본토 휴대전화 번호와 6자리 SMS 인증 코드로 로그인합니다.",
            "Access Token을 입력해 계정을 추가할 수 있으며, Refresh Token을 함께 입력하면 자동 갱신을 지원합니다.",
            "AutoClaw 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있으며, 전체 로그인 정보가 포함되어 있으면 자동 갱신을 지원합니다.",
        ),
        "pt-BR" => (
            ["Entrar por SMS", "Telefone da China continental", "Inserir credenciais", "Importar JSON", "Credenciais JSON"],
            "Informe um número da China continental e o código SMS de 6 dígitos para entrar.",
            "Informe o Access Token para adicionar a conta; inclua o Refresh Token para ativar a renovação automática.",
            "Selecione ou cole um arquivo de credenciais do AutoClaw para importar a conta; a renovação automática é suportada quando os dados completos de login estão incluídos.",
        ),
        "zh-Hant" => (
            ["手機驗證碼登入", "中國大陸手機號", "手動填寫憑證", "匯入 JSON", "憑證 JSON"],
            "輸入中國大陸手機號碼與 6 位簡訊驗證碼即可登入。",
            "填寫 Access Token 即可新增帳號；填寫 Refresh Token 後支援自動續期。",
            "選擇或貼上 AutoClaw 憑據檔案即可匯入帳號；包含完整登入資訊時支援自動續期。",
        ),
        _ => (
            ["手机验证码登录", "中国大陆手机号", "手动填写凭据", "导入 JSON", "凭据 JSON"],
            "输入中国大陆手机号与 6 位短信验证码即可登录。",
            "填写 Access Token 即可添加账号；填写 Refresh Token 后支持自动续期。",
            "选择或粘贴 AutoClaw 凭据文件即可导入账号；包含完整登录信息时支持自动续期。",
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
    let desktop = agent2api_wasm_sdk::login::desktop_method(3, "AutoClaw", locale);
    json!({ "methods": [
        sms_method,
        { "methodIndex": 1, "type": "api", "label": text[2], "instructions": manual_hint, "prompts": [
            { "key": "token", "type": "text", "label": "Access Token", "secret": true },
            { "key": "refreshToken", "type": "text", "label": "Refresh Token", "secret": true, "optional": true },
            { "key": "deviceId", "type": "text", "label": "Device ID", "optional": true }
        ] },
        { "methodIndex": 2, "type": "api", "label": text[3], "instructions": file_hint, "prompts": [
            { "key": "credentialJson", "type": "text", "label": text[4], "secret": true }
        ] },
        desktop,
        agent2api_wasm_sdk::legacy_credentials::method(4, locale)
    ] })
}
