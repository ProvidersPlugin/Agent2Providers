//! 密码、令牌、短信和 JSON 索引保留；本机导入使用独立的 desktop 类型。

use agent2api_wasm_sdk::{
    json,
    login::sms::{self, Config, CountryCode},
    Value,
};

pub fn methods(locale: &str) -> Value {
    let text = match locale {
        "en" => ["Sign in with password", "Complete the Geetest slider. Your password is not saved.", "Paste modelBearerToken", "Enter modelBearerToken for chat; include web session fields if you also need points and daily check-in.", "SMS sign-in", "Import credentials JSON", "Select or paste an AStudio credential file to import the account.", "Phone number", "11-digit mainland China mobile number"],
        "ja" => ["パスワードでログイン", "極験のスライダーを完了します。パスワードは保存されません。", "modelBearerToken を貼り付け", "modelBearerToken のみで会話できます。ポイント照会や日次チェックインが必要な場合は Web セッション情報も入力してください。", "SMSでログイン", "認証情報JSONを取り込む", "AStudio の認証ファイルを選択または貼り付けてアカウントを取り込めます。", "電話番号", "中国本土の11桁携帯番号"],
        "ko" => ["비밀번호 로그인", "Geetest 슬라이더 인증을 완료하세요. 비밀번호는 저장되지 않습니다.", "modelBearerToken 붙여넣기", "대화에는 modelBearerToken만 필요하며, 포인트 조회와 일일 출석이 필요하면 웹 세션 정보도 함께 입력하세요.", "SMS 로그인", "인증 JSON 가져오기", "AStudio 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있습니다.", "휴대전화 번호", "중국 본토 11자리 휴대전화 번호"],
        "pt-BR" => ["Entrar com senha", "Conclua o Geetest. Sua senha não é salva.", "Colar modelBearerToken", "Informe o modelBearerToken para conversar; preencha os dados da sessão web se também precisar de pontos e check-in diário.", "Entrar por SMS", "Importar credenciais JSON", "Selecione ou cole um arquivo de credenciais do AStudio para importar a conta.", "Celular", "Número da China continental com 11 dígitos"],
        "zh-Hant" => ["帳號密碼登入", "完成極驗滑塊；密碼不會保存。", "貼上 modelBearerToken", "填寫 modelBearerToken 即可對話；如需積分查詢與每日簽到，請一併填寫網頁會話資訊。", "手機簡訊登入", "匯入憑證 JSON", "選擇或貼上 AStudio 憑據檔案即可匯入帳號。", "手機號碼", "中國大陸11位手機號碼"],
        _ => ["账号密码登录", "完成极验滑块；密码不会保存。", "粘贴 modelBearerToken", "填写 modelBearerToken 即可对话；如需积分查询与每日签到，请一并填写网页会话信息。", "手机短信登录", "导入凭据 JSON", "选择或粘贴 AStudio 凭据文件即可导入账号。", "手机号", "中国大陆11位手机号"],
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
