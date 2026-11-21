//! 登录页负责显示真实微信二维码；本机导入与网页登录各用独立提交入口。

use agent2api_wasm_sdk::{json, Value};

pub fn methods(locale: &str) -> Value {
    let (scan, scan_hint, manual, placeholder, import, import_hint) = match locale {
        "en" => (
            "Sign in with WeChat", "Scan the QR code on the official OrcaTerm authorization page to add the account automatically.",
            "Enter credentials", "Paste an OrcaTerm JWT or data.bin JSON", "Import credentials text",
            "Select or paste an OrcaTerm credential file (such as data.bin) or JWT to import the account.",
        ),
        "zh-Hant" => (
            "微信掃碼登入", "在 OrcaTerm 官方授權頁面用微信掃碼即可自動新增帳號。",
            "貼上登入憑證", "貼上 OrcaTerm JWT 或完整 data.bin JSON", "匯入憑證文字",
            "選擇或貼上 OrcaTerm 憑據檔案（如 data.bin）或 JWT 即可匯入帳號。",
        ),
        "ja" => (
            "WeChat でログイン", "OrcaTerm の公式認証ページで QR コードを読み取ると自動でアカウントを追加します。",
            "認証情報を入力", "OrcaTerm JWT または data.bin の JSON 全体を貼り付け", "認証テキストを取り込む",
            "OrcaTerm の認証ファイル（data.bin など）または JWT を選択・貼り付けてアカウントを取り込めます。",
        ),
        "ko" => (
            "WeChat으로 로그인", "OrcaTerm 공식 인증 페이지에서 QR 코드를 스캔하면 계정이 자동 추가됩니다.",
            "인증 정보 입력", "OrcaTerm JWT 또는 전체 data.bin JSON 붙여넣기", "인증 텍스트 가져오기",
            "OrcaTerm 인증 파일(data.bin 등) 또는 JWT를 선택하거나 붙여넣어 계정을 가져올 수 있습니다.",
        ),
        "pt-BR" => (
            "Entrar com WeChat", "Leia o QR code na página oficial de autorização do OrcaTerm para adicionar a conta automaticamente.",
            "Inserir credenciais", "Cole um JWT do OrcaTerm ou o JSON completo de data.bin", "Importar texto de credenciais",
            "Selecione ou cole um arquivo de credenciais do OrcaTerm (como data.bin) ou JWT para importar a conta.",
        ),
        _ => (
            "微信扫码登录", "在 OrcaTerm 官方授权页面用微信扫码即可自动添加账号。",
            "粘贴登录凭据", "粘贴 OrcaTerm JWT 或完整 data.bin JSON", "导入凭据文本",
            "选择或粘贴 OrcaTerm 凭据文件（如 data.bin）或 JWT 即可导入账号。",
        ),
    };
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": scan, "prompts": [],
            "browserMode": "choice", "embeddedOnly": false, "manualCallback": false,
            "validatesCallback": true, "callbackInput": "url", "instructions": scan_hint },
        { "methodIndex": 1, "type": "api", "label": manual, "prompts": [
            { "key": "accessToken", "type": "text", "label": "Access Token", "secret": true, "placeholder": placeholder }
          ], "instructions": import_hint },
        { "methodIndex": 2, "type": "api", "label": import, "prompts": [
            { "key": "credentialText", "type": "text", "label": "data.bin / JWT", "secret": true }
          ], "instructions": import_hint },
        agent2api_wasm_sdk::login::desktop_method(3, "OrcaTerm", locale)
    ] })
}
