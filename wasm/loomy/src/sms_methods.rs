//! 方法索引只追加；本机读取与密码均是凭据入口，不伪装成等待 OAuth 回调。
use agent2api_wasm_sdk::{
    json,
    login::sms::{self, Config, CountryCode},
    Value,
};

pub fn methods(locale: &str) -> Value {
    let (sms_label, phone, phone_hint, credentials, user, import, hint, password, login, password_hint) = match locale {
        "en" => ("Sign in with SMS", "Phone number", "11-digit mainland China number", "Enter credentials", "User ID", "Import credentials JSON", "Paste a Session or a login-state JSON. A missing user ID is read from the official account service without spending points. Session expiry is not estimated; sign in again when rejected.", "Sign in with password", "Account / phone", "Uses the official account sign-in. The password is used only for this sign-in and is not stored; automatic password re-login is not enabled."),
        "zh-Hant" => ("手機驗證碼登入", "手機號", "11 位中國大陸手機號", "填寫憑證", "使用者 ID", "匯入憑證 JSON", "貼上 Session 或登入狀態 JSON；缺少使用者 ID 時僅向官方唯讀查詢，不消耗積分。不估算到期時間，上游拒絕後再重新登入。", "帳號密碼登入", "帳號／手機號", "使用官方帳號登入；密碼僅用於本次登入，不儲存，也不啟用自動密碼重登。"),
        "ja" => ("SMS でログイン", "電話番号", "中国本土の 11 桁の携帯電話番号", "認証情報を入力", "ユーザー ID", "認証 JSON を取り込む", "Session またはログイン状態の JSON を貼り付けます。ID がない場合はポイントを使わず公式アカウント情報を読み取ります。有効期限は推測せず、拒否されたら再ログインします。", "パスワードでログイン", "アカウント／電話番号", "公式のアカウント認証を使います。パスワードは今回のみ使用し保存しません。自動再ログインは有効にしません。"),
        "ko" => ("SMS로 로그인", "전화번호", "중국 본토 11자리 휴대전화 번호", "인증 정보 입력", "사용자 ID", "인증 JSON 가져오기", "Session 또는 로그인 상태 JSON을 붙여 넣으세요. ID가 없으면 포인트를 소비하지 않고 공식 계정 정보를 조회합니다. 만료 시점을 추정하지 않으며 거부되면 다시 로그인하세요.", "비밀번호로 로그인", "계정 / 전화번호", "공식 계정 로그인을 사용합니다. 비밀번호는 이번 로그인에만 쓰고 저장하지 않으며 자동 재로그인도 활성화하지 않습니다."),
        "pt-BR" => ("Entrar por SMS", "Telefone", "Número da China continental com 11 dígitos", "Inserir credenciais", "ID do usuário", "Importar JSON de credenciais", "Cole uma Session ou o JSON de login. Se faltar o ID, os dados oficiais são consultados sem gastar pontos. A expiração não é estimada; entre novamente quando a sessão for recusada.", "Entrar com senha", "Conta / telefone", "Usa o login oficial da conta. A senha serve apenas para este acesso, não é armazenada e não ativa uma nova entrada automática."),
        _ => ("手机验证码登录", "手机号", "11 位大陆手机号", "填写凭据", "用户 ID", "导入凭据 JSON", "粘贴 Session 或登录态 JSON；缺少用户 ID 时仅向官方只读查询，不消耗积分。不估算到期时间，上游拒绝后再重新登录。", "账号密码登录", "账号／手机号", "使用官方账号登录；密码只用于本次登录，不保存，也不启用自动密码重登。"),
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
        { "methodIndex": 2, "type": "api", "label": import, "instructions": hint, "prompts": [
            { "key": "credentialText", "type": "text", "label": "JSON", "secret": true }
        ] },
        agent2api_wasm_sdk::login::desktop_method(3, "Loomy", locale),
        { "methodIndex": 4, "type": "api", "label": password, "instructions": password_hint, "prompts": [
            { "key": "loginId", "type": "text", "label": login },
            { "key": "password", "type": "text", "label": password, "secret": true }
        ] }
    ] })
}
