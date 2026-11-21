//! 方法索引只追加；本机读取与密码均是凭据入口，不伪装成等待 OAuth 回调。
use agent2api_wasm_sdk::{
    json,
    login::sms::{self, Config, CountryCode},
    Value,
};

pub fn methods(locale: &str) -> Value {
    let (sms_label, phone, phone_hint, credentials, user, import, hint, file_hint, password, login, password_hint) = match locale {
        "en" => ("Sign in with SMS", "Phone number", "11-digit mainland China number", "Enter credentials", "User ID", "Import credentials JSON", "Enter a Loomy Session to add the account.", "Select or paste a Loomy credential file to import the account.", "Sign in with password", "Account / phone", "Sign in with your Loomy account and password; your password is not saved."),
        "zh-Hant" => ("手機驗證碼登入", "手機號", "11 位中國大陸手機號", "填寫憑證", "使用者 ID", "匯入憑證 JSON", "填寫 Loomy Session 即可新增帳號。", "選擇或貼上 Loomy 憑據檔案即可匯入帳號。", "帳號密碼登入", "帳號／手機號", "使用官方帳號與密碼登入；密碼不會保存。"),
        "ja" => ("SMS でログイン", "電話番号", "中国本土の 11 桁の携帯電話番号", "認証情報を入力", "ユーザー ID", "認証 JSON を取り込む", "Loomy の Session を入力してアカウントを追加できます。", "Loomy の認証ファイルを選択または貼り付けてアカウントを取り込めます。", "パスワードでログイン", "アカウント／電話番号", "公式アカウントとパスワードでログインします。パスワードは保存しません。"),
        "ko" => ("SMS로 로그인", "전화번호", "중국 본토 11자리 휴대전화 번호", "인증 정보 입력", "사용자 ID", "인증 JSON 가져오기", "Loomy Session을 입력해 계정을 추가할 수 있습니다.", "Loomy 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있습니다.", "비밀번호로 로그인", "계정 / 전화번호", "공식 계정과 비밀번호로 로그인하며 비밀번호는 저장하지 않습니다."),
        "pt-BR" => ("Entrar por SMS", "Telefone", "Número da China continental com 11 dígitos", "Inserir credenciais", "ID do usuário", "Importar JSON de credenciais", "Informe a Session do Loomy para adicionar a conta.", "Selecione ou cole um arquivo de credenciais do Loomy para importar a conta.", "Entrar com senha", "Conta / telefone", "Entre com sua conta e senha do Loomy; a senha não é salva."),
        _ => ("手机验证码登录", "手机号", "11 位大陆手机号", "填写凭据", "用户 ID", "导入凭据 JSON", "填写 Loomy Session 即可添加账号。", "选择或粘贴 Loomy 凭据文件即可导入账号。", "账号密码登录", "账号／手机号", "使用官方账号与密码登录；密码不会保存。"),
    };
    let sms = sms::method(
        0,
        sms_label,
        Config {
            phone_label: phone.into(),
            phone_placeholder: phone_hint.into(),
            country_codes: vec![CountryCode {
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
    json!({ "methods": [sms,
        { "methodIndex": 1, "type": "api", "label": credentials, "instructions": hint, "prompts": [
            { "key": "session", "type": "text", "label": "Session", "secret": true },
            { "key": "phone", "type": "text", "label": phone, "optional": true },
            { "key": "userId", "type": "text", "label": user, "optional": true }
        ] },
        { "methodIndex": 2, "type": "api", "label": import, "instructions": file_hint, "prompts": [
            { "key": "credentialText", "type": "text", "label": "JSON", "secret": true }
        ] },
        agent2api_wasm_sdk::login::desktop_method(3, "Loomy", locale),
        { "methodIndex": 4, "type": "api", "label": password, "instructions": password_hint, "prompts": [
            { "key": "loginId", "type": "text", "label": login },
            { "key": "password", "type": "text", "label": password, "secret": true }
        ] }
    ] })
}
