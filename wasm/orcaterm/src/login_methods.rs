//! 登录页负责显示真实微信二维码；本机导入与网页登录各用独立提交入口。

use agent2api_wasm_sdk::{json, Value};

pub fn methods(locale: &str) -> Value {
    let (scan, scan_hint, manual, placeholder, import, import_hint) = match locale {
        "en" => (
            "Sign in with WeChat", "Scan the QR code on the official OrcaTerm authorization page. This app checks the result automatically; no callback URL needs copying. The embedded window and system browser are both available.",
            "Enter credentials", "Paste an OrcaTerm JWT or the full data.bin JSON", "Import credentials text",
            "Paste the contents of data.bin, or a JWT prefixed with Bearer or ot_session=.",
        ),
        "zh-Hant" => (
            "微信掃碼登入", "在 OrcaTerm 官方授權頁面用微信掃碼；應用會自動核對結果並回填帳號，無須複製回呼地址。可使用內嵌視窗或系統瀏覽器。",
            "貼上登入憑證", "貼上 OrcaTerm JWT 或完整 data.bin JSON", "匯入憑證文字",
            "貼上 data.bin 的完整內容，也支援 Bearer 或 ot_session= 前綴的 JWT。",
        ),
        "ja" => (
            "WeChat でログイン", "OrcaTerm の公式認証ページで QR コードを読み取ってください。認証結果は自動確認されるため、コールバック URL のコピーは不要です。埋め込みウィンドウとシステムブラウザーを利用できます。",
            "認証情報を入力", "OrcaTerm JWT または data.bin の JSON 全体を貼り付け", "認証テキストを取り込む",
            "data.bin の内容、または Bearer / ot_session= で始まる JWT を貼り付けてください。",
        ),
        "ko" => (
            "WeChat으로 로그인", "OrcaTerm 공식 인증 페이지에서 QR 코드를 스캔하세요. 앱이 결과를 자동으로 확인하므로 콜백 주소를 복사할 필요가 없습니다. 내장 창과 시스템 브라우저를 사용할 수 있습니다.",
            "인증 정보 입력", "OrcaTerm JWT 또는 전체 data.bin JSON 붙여넣기", "인증 텍스트 가져오기",
            "data.bin 내용 또는 Bearer / ot_session= 접두사가 있는 JWT를 붙여넣으세요.",
        ),
        "pt-BR" => (
            "Entrar com WeChat", "Leia o QR code na página oficial de autorização do OrcaTerm. O aplicativo verifica o resultado automaticamente; não é necessário copiar a URL de retorno. Use a janela integrada ou o navegador do sistema.",
            "Inserir credenciais", "Cole um JWT do OrcaTerm ou o JSON completo de data.bin", "Importar texto de credenciais",
            "Cole o conteúdo de data.bin ou um JWT com o prefixo Bearer ou ot_session=.",
        ),
        _ => (
            "微信扫码登录", "在 OrcaTerm 官方授权页面用微信扫码；应用会自动核对结果并回填账号，无需复制回调地址。可使用内嵌窗口或系统浏览器。",
            "粘贴登录凭据", "粘贴 OrcaTerm JWT 或完整 data.bin JSON", "导入凭据文本",
            "粘贴 data.bin 的完整内容，也支持 Bearer 或 ot_session= 前缀的 JWT。",
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
