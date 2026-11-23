//! 保持原生三种入口及新增独立JSON导入项；未就绪能力显示真实原因而不是消失。

use agent2api_wasm_sdk::{json, Value};

pub const WEB: &str = "在打开的授权页完成登录即可添加账号；若门户选择 AWS 会进入第二步授权，远程访问面板时按提示粘贴回调地址即可。";
pub fn methods(locale: &str) -> Value {
    let mut value = json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": "Kiro 网页登录",
            "instructions": WEB, "localOnly": false, "manualCallback": true,
            "callbackInput": "url", "validatesCallback": true, "browserMode": "choice",
            "prompts": [
                { "key": "loginMethod", "type": "select", "label": "登录方式", "default": "portal", "options": [
                    { "value": "portal", "label": "Kiro登录页（Google / GitHub / AWS）" },
                    { "value": "builderid", "label": "AWS Builder ID" },
                    { "value": "enterprise", "label": "IAM Identity Center" } ] },
                { "key": "startUrl", "type": "text", "label": "组织登录地址", "placeholder": "https://d-xxxxxxxxxx.awsapps.com/start",
                    "when": { "key": "loginMethod", "op": "eq", "value": "enterprise" } },
                { "key": "region", "type": "text", "label": "AWS目录地区", "default": "us-east-1", "optional": true,
                    "when": { "key": "loginMethod", "op": "eq", "value": "enterprise" } }
            ] },
        agent2api_wasm_sdk::login::desktop_method(1, "Kiro CLI / Kiro IDE", locale),
        { "methodIndex": 2, "type": "api", "label": "Kiro API key", "instructions": "粘贴 ksk_ 开头的 API Key 即可添加账号。",
            "prompts": [{ "key": "apiKey", "type": "text", "label": "API key", "secret": true, "placeholder": "ksk_…" },
                { "key": "name", "type": "text", "label": "备注名", "optional": true }] },
        { "methodIndex": 3, "type": "api", "label": "导入凭据JSON",
            "instructions": "选择或粘贴 Kiro 凭据文件即可导入账号；包含完整登录信息时支持自动续期。",
            "prompts": [{ "key": "credentialJson", "type": "text", "label": "凭据JSON", "secret": true },
                { "key": "name", "type": "text", "label": "备注名", "optional": true }] }
    ] });
    translate(&mut value, locale);
    value
}

fn translate(value: &mut Value, locale: &str) {
    let column = match locale {
        "en" => 1,
        "ja" => 2,
        "ko" => 3,
        "pt-BR" => 4,
        "zh-Hant" => 5,
        _ => return,
    };
    let translations = [
        ["Kiro 网页登录", "Kiro web sign-in", "Kiro ウェブログイン", "Kiro 웹 로그인", "Login web do Kiro", "Kiro 網頁登入"],
        [WEB,
         "Sign in on the authorization page to add the account; choosing AWS opens a second authorization step. When accessing the panel remotely, paste the callback URL as prompted.",
         "認証ページでログインするとアカウントを追加できます。AWS を選んだ場合は2段階目の認証に進みます。リモート画面では案内に従ってコールバック URL を貼り付けてください。",
         "인증 페이지에서 로그인하면 계정이 추가됩니다. AWS를 선택하면 2단계 인증이 진행되며, 원격 패널에서는 안내에 따라 콜백 URL을 붙여넣으세요.",
         "Conclua o login na página de autorização para adicionar a conta; escolher AWS abre uma segunda etapa. No acesso remoto ao painel, cole a URL de callback conforme solicitado.",
         "在開啟的授權頁完成登入即可新增帳號；若選擇 AWS 會進入第二步授權，遠端存取面板時按提示貼上回呼網址即可。"],
        ["登录方式", "Sign-in method", "ログイン方法", "로그인 방식", "Método de login", "登入方式"],
        ["Kiro登录页（Google / GitHub / AWS）", "Kiro portal (Google / GitHub / AWS)", "Kiro ログインページ（Google / GitHub / AWS）", "Kiro 로그인 페이지 (Google / GitHub / AWS)", "Portal do Kiro (Google / GitHub / AWS)", "Kiro 登入頁（Google / GitHub / AWS）"],
        ["组织登录地址", "Organization sign-in URL", "組織ログイン URL", "조직 로그인 주소", "URL de login da organização", "組織登入網址"],
        ["AWS目录地区", "AWS directory region", "AWS ディレクトリのリージョン", "AWS 디렉터리 리전", "Região do diretório AWS", "AWS 目錄區域"],
        ["粘贴 ksk_ 开头的 API Key 即可添加账号。", "Paste an API key starting with ksk_ to add the account.", "ksk_ で始まる API キーを貼り付けてアカウントを追加できます。", "ksk_로 시작하는 API 키를 붙여넣어 계정을 추가할 수 있습니다.", "Cole uma chave de API que comece com ksk_ para adicionar a conta.", "貼上 ksk_ 開頭的 API Key 即可新增帳號。"],
        ["备注名", "Display name", "表示名", "표시 이름", "Nome de exibição", "備註名稱"],
        ["导入凭据JSON", "Import credentials JSON", "認証情報 JSON を取り込む", "인증 정보 JSON 가져오기", "Importar JSON de credenciais", "匯入憑據 JSON"],
        ["选择或粘贴 Kiro 凭据文件即可导入账号；包含完整登录信息时支持自动续期。",
         "Select or paste a Kiro credential file to import the account; automatic renewal is supported when complete sign-in details are included.",
         "Kiro の認証ファイルを選択または貼り付けてアカウントを取り込めます。完全なログイン情報が含まれていれば自動更新に対応します。",
         "Kiro 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있으며, 전체 로그인 정보가 포함되어 있으면 자동 갱신을 지원합니다.",
         "Selecione ou cole um arquivo de credenciais do Kiro para importar a conta; a renovação automática é suportada quando os dados completos de login estão incluídos.",
         "選擇或貼上 Kiro 憑據檔案即可匯入帳號；包含完整登入資訊時支援自動續期。"],
        ["凭据JSON", "Credentials JSON", "認証情報 JSON", "인증 정보 JSON", "JSON de credenciais", "憑據 JSON"],
    ];
    fn walk(value: &mut Value, translations: &[[&str; 6]], column: usize) {
        match value {
            Value::String(text) => {
                if let Some(row) = translations.iter().find(|row| row[0] == text.as_str()) {
                    *text = row[column].to_string();
                }
            }
            Value::Array(values) => values
                .iter_mut()
                .for_each(|value| walk(value, translations, column)),
            Value::Object(fields) => fields
                .values_mut()
                .for_each(|value| walk(value, translations, column)),
            _ => {}
        }
    }
    walk(value, &translations, column);
}
