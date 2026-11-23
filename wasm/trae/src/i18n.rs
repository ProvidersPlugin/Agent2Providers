//! 原手工/JSON入口保留；方式0由宿主持有PKCE，支持内嵌与远程完整回调。

use agent2api_wasm_sdk::{json, Value};

pub const WEB: &str = "在官方页面完成 Trae 授权即可添加账号；远程访问面板且未自动返回时，按提示粘贴回调地址即可。";

fn texts(locale: &str) -> [&'static str; 8] {
    match locale {
        "en" => ["Managed web sign-in", "Complete official Trae authorization to add the account; when accessing the panel remotely, paste the callback URL if it does not return automatically.", "Paste credentials", "Import credentials JSON", "Enter an Access Token to add the account; include a Refresh Token and device keys to enable automatic renewal.", "Credentials JSON", "Trae app (China)", "Select or paste a Trae credential file to import the account; automatic renewal is supported when complete sign-in details are included."],
        "zh-Hant" => ["託管網頁登入", "在官方頁面完成 Trae 授權即可新增帳號；遠端存取面板且未自動返回時，按提示貼上回呼網址即可。", "貼上憑證", "匯入憑證JSON", "填寫 Access Token 即可新增帳號；一併填寫 Refresh Token 與裝置金鑰時支援自動續期。", "憑證JSON", "Trae 應用（中國版）", "選擇或貼上 Trae 憑據檔案即可匯入帳號；包含完整登入資訊時支援自動續期。"],
        "ja" => ["管理されたウェブログイン", "公式ページで Trae 認証を完了するとアカウントを追加できます。リモート画面で自動的に戻らない場合はコールバック URL を貼り付けてください。", "認証情報を貼り付け", "認証JSONを取り込む", "Access Token を入力してアカウントを追加できます。Refresh Token とデバイス鍵を入力すると自動更新に対応します。", "認証JSON", "Trae アプリ（中国版）", "Trae の認証ファイルを選択または貼り付けてアカウントを取り込めます。完全なログイン情報が含まれていれば自動更新に対応します。"],
        "ko" => ["관리형 웹 로그인", "공식 페이지에서 Trae 인증을 완료하면 계정이 추가됩니다. 원격 패널에서 자동으로 돌아오지 않으면 콜백 URL을 붙여넣으세요.", "인증 정보 붙여넣기", "인증 JSON 가져오기", "Access Token을 입력해 계정을 추가할 수 있으며, Refresh Token과 기기 키를 함께 입력하면 자동 갱신을 지원합니다.", "인증 JSON", "Trae 앱(중국판)", "Trae 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있으며, 전체 로그인 정보가 포함되어 있으면 자동 갱신을 지원합니다."],
        "pt-BR" => ["Login gerenciado pela web", "Conclua a autorização oficial do Trae para adicionar a conta; no acesso remoto ao painel, cole a URL de callback caso não retorne automaticamente.", "Colar credenciais", "Importar credenciais JSON", "Informe o Access Token para adicionar a conta; inclua o Refresh Token e as chaves do dispositivo para ativar a renovação automática.", "Credenciais JSON", "Aplicativo Trae (China)", "Selecione ou cole um arquivo de credenciais do Trae para importar a conta; a renovação automática é suportada quando os dados completos de login estão incluídos."],
        _ => ["托管网页登录", WEB, "粘贴凭据", "导入凭据JSON", "填写 Access Token 即可添加账号；一并填写 Refresh Token 与设备密钥时支持自动续期。", "凭据JSON", "Trae 应用（国内）", "选择或粘贴 Trae 凭据文件即可导入账号；包含完整登录信息时支持自动续期。"],
    }
}

pub fn web(locale: &str) -> &'static str {
    texts(locale)[1]
}
pub fn web_title(locale: &str) -> &'static str {
    texts(locale)[0]
}

pub fn methods(locale: &str) -> Value {
    let t = texts(locale);
    let variants = json!([
        { "value": "solo", "label": "Trae Solo (CN)" },
        { "value": "cn", "label": "Trae IDE (CN)" }
    ]);
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": t[0], "instructions": t[1], "prompts": [
            { "key": "variant", "type": "select", "label": t[6], "default": "solo", "options": variants }
        ], "browserMode": "choice", "manualCallback": true, "callbackInput": "url", "validatesCallback": true },
        { "methodIndex": 1, "type": "api", "label": t[2], "instructions": t[4], "prompts": [
            { "key": "accessToken", "type": "text", "label": "accessToken", "secret": true, "optional": true },
            { "key": "refreshToken", "type": "text", "label": "refreshToken", "secret": true, "optional": true },
            { "key": "uid", "type": "text", "label": "uid", "optional": true },
            { "key": "deviceId", "type": "text", "label": "deviceId", "secret": true, "optional": true },
            { "key": "machineId", "type": "text", "label": "machineId", "secret": true, "optional": true },
            { "key": "authClientId", "type": "text", "label": "authClientId", "optional": true },
            { "key": "variant", "type": "select", "label": t[6], "default": "solo", "options": variants },
            { "key": "devicePublicKey", "type": "text", "label": "Device public key (PEM)", "secret": true, "optional": true },
            { "key": "devicePrivateKey", "type": "text", "label": "Device private key (PEM)", "secret": true, "optional": true }
        ] },
        { "methodIndex": 2, "type": "api", "label": t[3], "instructions": t[7], "prompts": [
            { "key": "credentialJson", "type": "text", "label": t[5], "secret": true }
        ] }
    ] })
}
