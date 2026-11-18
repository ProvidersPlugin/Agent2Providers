//! 密码、令牌、短信和 JSON 索引保留；本机导入使用独立的 desktop 类型。

use agent2api_wasm_sdk::{
    json,
    login::sms::{self, Config, CountryCode},
    Value,
};

pub fn methods(locale: &str) -> Value {
    let text = match locale {
        "en" => ["Sign in with password", "Complete the Geetest slider. Your password is not saved.", "Paste modelBearerToken", "A web session is needed for points and daily rewards. The account session token is not a model key. If config.toml has a separate chat key, supply it below; there is no automatic renewal.", "SMS sign-in", "Import credentials JSON", "Paste astron-session.json or an exported AStudio account.", "Phone number", "11-digit mainland China mobile number"],
        "ja" => ["パスワードでログイン", "極験のスライダーを完了します。パスワードは保存されません。", "modelBearerToken を貼り付け", "ポイントと毎日の報酬には Web セッションが必要です。アカウントのセッショントークンはモデルキーではありません。config.toml に別のチャットキーがある場合は下に入力してください。自動更新には対応していません。", "SMSでログイン", "認証情報JSONを取り込む", "astron-session.json または AStudio アカウントJSONを貼り付けます。", "電話番号", "中国本土の11桁携帯番号"],
        "ko" => ["비밀번호 로그인", "Geetest 슬라이더 인증을 완료하세요. 비밀번호는 저장되지 않습니다.", "modelBearerToken 붙여넣기", "포인트와 일일 보상에는 웹 세션이 필요합니다. 계정 세션 토큰은 모델 키가 아닙니다. config.toml에 별도 채팅 키가 있다면 아래에 입력하세요. 자동 갱신은 지원하지 않습니다.", "SMS 로그인", "인증 JSON 가져오기", "astron-session.json 또는 내보낸 AStudio 계정을 붙여넣으세요.", "휴대전화 번호", "중국 본토 11자리 휴대전화 번호"],
        "pt-BR" => ["Entrar com senha", "Conclua o Geetest. Sua senha não é salva.", "Colar modelBearerToken", "Pontos e recompensas diárias exigem uma sessão web. O token da sessão da conta não é uma chave de modelo. Se config.toml tiver uma chave de chat separada, informe-a abaixo; não há renovação automática.", "Entrar por SMS", "Importar credenciais JSON", "Cole astron-session.json ou uma conta AStudio exportada.", "Celular", "Número da China continental com 11 dígitos"],
        "zh-Hant" => ["帳號密碼登入", "完成極驗滑塊；密碼不會保存。", "貼上 modelBearerToken", "積分與每日獎勵需要網頁會話；帳號會話 token 不是模型憑證。若 config.toml 另有對話憑證，請在下方填寫；不支援自動續期。", "手機簡訊登入", "匯入憑證 JSON", "貼上 astron-session.json 或匯出的 AStudio 帳號。", "手機號碼", "中國大陸11位手機號碼"],
        _ => ["账号密码登录", "完成极验滑块；密码不会保存。", "粘贴 modelBearerToken", "积分与每日奖励需要网页会话；账号会话 token 不是模型凭据。若 config.toml 另有对话凭据，请在下方填写；不支持自动续期。", "手机短信登录", "导入凭据 JSON", "粘贴 astron-session.json 或导出的 AStudio 账号。", "手机号", "中国大陆11位手机号"],
    };
    let chat_key = match locale {
        "en" => "Separate chat key from config.toml (optional)",
        "ja" => "config.toml の別のチャットキー（任意）",
        "ko" => "config.toml의 별도 채팅 키 (선택)",
        "pt-BR" => "Chave de chat separada de config.toml (opcional)",
        "zh-Hant" => "config.toml 的獨立對話憑證（可選）",
        _ => "config.toml 的独立对话凭据（可选）",
    };
    let sms = sms::method(
        3,
        text[4],
        Config {
            phone_label: text[7].into(),
            phone_placeholder: text[8].into(),
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
        { "methodIndex": 1, "type": "oauth", "label": text[0], "instructions": text[1],
            "prompts": [], "browserMode": "none", "manualCallback": false },
        { "methodIndex": 2, "type": "api", "label": text[2], "instructions": text[3], "prompts": [
            { "key": "modelBearerToken", "type": "text", "label": "modelBearerToken", "secret": true },
            { "key": "key", "type": "text", "label": chat_key, "secret": true, "optional": true },
            { "key": "uid", "type": "text", "label": "uid", "optional": true },
            { "key": "sessionToken", "type": "text", "label": "sessionToken", "secret": true, "optional": true },
            { "key": "accountId", "type": "text", "label": "accountId", "optional": true },
            { "key": "ssoSessionId", "type": "text", "label": "ssoSessionId", "secret": true, "optional": true }
        ] },
        { "methodIndex": 4, "type": "api", "label": text[5], "instructions": text[6], "prompts": [
            { "key": "credentialJson", "type": "text", "label": "JSON", "secret": true }
        ] },
        agent2api_wasm_sdk::login::desktop_method(0, "AStudio", locale)
    ] })
}

pub fn password_text(locale: &str) -> [&'static str; 4] {
    match locale {
        "en" => ["AStudio password sign-in", "Enter your account and password, then complete the Geetest slider. Your password is not saved.", "Phone / email / username", "Password (6–20 characters)"],
        "ja" => ["AStudio パスワードログイン", "アカウントとパスワードを入力し、極験のスライダーを完了してください。パスワードは保存されません。", "電話番号 / メール / ユーザー名", "パスワード（6～20文字）"],
        "ko" => ["AStudio 비밀번호 로그인", "계정과 비밀번호를 입력하고 Geetest 슬라이더 인증을 완료하세요. 비밀번호는 저장되지 않습니다.", "전화번호 / 이메일 / 사용자 이름", "비밀번호 (6–20자)"],
        "pt-BR" => ["Login com senha do AStudio", "Informe sua conta e senha e conclua o Geetest. A senha não é salva.", "Telefone / e-mail / usuário", "Senha (6–20 caracteres)"],
        "zh-Hant" => ["AStudio 帳號密碼登入", "填寫帳號與密碼並完成極驗滑塊；密碼不會保存", "手機號 / 郵箱 / 使用者名稱", "密碼（6–20位）"],
        _ => ["AStudio 账号密码登录", "填写账号与密码并完成极验滑块；密码不会保存", "手机号 / 邮箱 / 用户名", "密码（6–20位）"],
    }
}
