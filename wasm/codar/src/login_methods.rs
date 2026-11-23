use agent2api_wasm_sdk::{json, login::sms, Value};

use crate::endpoints;

pub fn methods(locale: &str) -> Value {
    let labels = match locale {
        "en" => ["WeChat QR sign-in", "Sign in with SMS", "Paste access token", "Import credentials JSON", "Link local Codar sign-in", "Account note", "Scan the QR code with WeChat to sign in.", "Complete verification and enter the SMS code to sign in.", "Select or paste a Codar credential file to import the account.", "Phone number", "11-digit mainland China phone number", "+86 Mainland China"],
        "zh-Hant" => ["微信掃碼登入", "手機驗證碼登入", "貼上存取權杖", "匯入憑據 JSON", "關聯本機科搭登入", "備註名稱", "使用微信掃碼即可登入並新增帳號。", "完成人機驗證後取得並輸入簡訊驗證碼即可登入。", "選擇或貼上科搭憑據檔案即可匯入帳號。", "手機號", "11 位中國大陸手機號", "+86 中國大陸"],
        "ja" => ["WeChat QR ログイン", "SMS でログイン", "アクセストークンを貼り付け", "認証 JSON を取り込む", "ローカル Codar ログインを関連付け", "メモ名", "WeChat で QR コードを読み取ってログインします。", "認証を完了し、SMS コードを取得・入力してログインします。", "Codar の認証ファイルを選択または貼り付けてアカウントを取り込めます。", "電話番号", "中国本土の11桁の電話番号", "+86 中国本土"],
        "ko" => ["WeChat QR 로그인", "SMS로 로그인", "액세스 토큰 붙여넣기", "인증 JSON 가져오기", "로컬 Codar 로그인 연결", "메모 이름", "WeChat으로 QR 코드를 스캔하여 로그인합니다.", "인증을 완료하고 SMS 코드를 받아 입력하여 로그인합니다.", "Codar 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있습니다.", "휴대전화 번호", "중국 본토 11자리 휴대전화 번호", "+86 중국 본토"],
        "pt-BR" => ["Entrar pelo QR do WeChat", "Entrar por SMS", "Colar token de acesso", "Importar JSON de credenciais", "Vincular login local do Codar", "Nome da conta", "Leia o QR code com o WeChat para entrar.", "Conclua a verificação e informe o código SMS para entrar.", "Selecione ou cole um arquivo de credenciais do Codar para importar a conta.", "Telefone", "Telefone da China continental com 11 dígitos", "+86 China continental"],
        _ => ["微信扫码登录", "手机验证码登录", "粘贴访问令牌", "导入凭据 JSON", "关联本机科搭登录态", "备注名", "使用微信扫码即可登录并添加账号。", "完成人机验证后获取并输入短信验证码即可登录。", "选择或粘贴科搭凭据文件即可导入账号。", "手机号", "11 位中国大陆手机号", "+86 中国大陆"],
    };
    let mut sms_method = sms::method_with_challenge(
        1,
        labels[1],
        sms::Config {
            phone_label: labels[9].into(),
            phone_placeholder: labels[10].into(),
            country_codes: vec![sms::CountryCode {
                value: "86".into(),
                label: labels[11].into(),
            }],
            phone_min_length: 11,
            phone_max_length: 11,
            code_min_length: 4,
            code_max_length: 8,
            resend_after_seconds: 60,
            expires_after_seconds: 300,
        },
        sms::Challenge::Alicom4 {
            captcha_id: endpoints::CAPTCHA_ID.into(),
        },
    );
    sms_method["instructions"] = json!(labels[7]);
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": labels[0], "prompts": [], "browserMode": "none", "manualCallback": false, "instructions": labels[6] },
        sms_method,
        { "methodIndex": 2, "type": "api", "label": labels[2], "prompts": [
            { "key": "token", "type": "text", "label": "Codar JWT", "secret": true },
            { "key": "name", "type": "text", "label": labels[5], "optional": true }
        ] },
        { "methodIndex": 3, "type": "api", "label": labels[3], "instructions": labels[8], "prompts": [
            { "key": "credentialJson", "type": "text", "label": "JSON", "secret": true },
            { "key": "name", "type": "text", "label": labels[5], "optional": true }
        ] },
        agent2api_wasm_sdk::login::desktop_method(4, "科搭 / Codar", locale)
    ] })
}
