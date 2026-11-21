//! 原手工/JSON入口保留；方式0由宿主持有PKCE，支持内嵌与远程完整回调。

use agent2api_wasm_sdk::{json, Value};

pub const WEB: &str = "在官方页面完成 Trae 授权。桌面可选择内置窗口或系统浏览器；远程面板授权后如未自动返回，请在当前步骤粘贴完整回调地址。授权码、PKCE 和设备密钥由主程序持有，只兑换一次；尚未完成真实账号验收。";

fn texts(locale: &str) -> [&'static str; 7] {
    match locale {
        "en" => ["Managed web sign-in", "Complete official Trae authorization. Desktop users can choose the embedded window or system browser; on a remote panel, paste the full callback URL into the current step if it does not return automatically. The host keeps the authorization code, PKCE and device keys for one exchange; real-account verification is pending.", "Paste credentials", "Import credentials JSON", "Keep the original device IDs and key pair. China accounts only. Automatic renewal may sign the official client out; refresh tokens are imported only through this authenticated form.", "Credentials JSON", "Trae app (China)"],
        "zh-Hant" => ["託管網頁登入", "在官方頁面完成 Trae 授權。桌面可選內嵌視窗或系統瀏覽器；遠端面板若未自動返回，請在目前步驟貼上完整回呼網址。授權碼、PKCE 與裝置金鑰由主程式持有，只交換一次；尚未完成真實帳號驗收。", "貼上憑證", "匯入憑證JSON", "保留原始裝置標識與成對金鑰。僅中國版帳號；自動續期可能使官方用戶端登出，續期憑證只能在已登入管理表單中匯入。", "憑證JSON", "Trae 應用（中國版）"],
        "ja" => ["管理されたウェブログイン", "Trae の公式認証を完了してください。デスクトップでは内蔵ウィンドウまたはシステムブラウザーを選択できます。リモート画面に自動で戻らない場合は、現在の手順に完全なコールバック URL を貼り付けてください。認可コード、PKCE とデバイス鍵はホストが保持し、交換は1回だけです。実アカウントでの検証は未完了です。", "認証情報を貼り付け", "認証JSONを取り込む", "元のデバイスIDと鍵ペアを保持します。中国版のみ。自動更新により公式クライアントがログアウトする場合があります。更新トークンは認証済み管理フォームでのみ取り込めます。", "認証JSON", "Trae アプリ（中国版）"],
        "ko" => ["관리형 웹 로그인", "Trae 공식 인증을 완료하세요. 데스크톱에서는 내장 창이나 시스템 브라우저를 선택할 수 있습니다. 원격 패널로 자동으로 돌아오지 않으면 현재 단계에 전체 콜백 URL을 붙여넣으세요. 인증 코드, PKCE와 기기 키는 호스트가 보관하며 한 번만 교환합니다. 실제 계정 검증은 아직 완료되지 않았습니다.", "인증 정보 붙여넣기", "인증 JSON 가져오기", "원래 기기 ID와 키 쌍을 유지하세요. 중국 계정만 지원합니다. 자동 갱신으로 공식 클라이언트가 로그아웃될 수 있으며 갱신 토큰은 인증된 관리 양식에서만 가져옵니다.", "인증 JSON", "Trae 앱(중국판)"],
        "pt-BR" => ["Login gerenciado pela web", "Conclua a autorização oficial do Trae. No desktop, escolha a janela integrada ou o navegador do sistema; no painel remoto, cole a URL completa de retorno na etapa atual se não houver retorno automático. O host mantém o código, o PKCE e as chaves do dispositivo para uma única troca; a validação com conta real está pendente.", "Colar credenciais", "Importar credenciais JSON", "Preserve os IDs e o par de chaves do dispositivo. Apenas contas da China. A renovação automática pode desconectar o cliente oficial; os tokens de renovação só são importados neste formulário autenticado.", "Credenciais JSON", "Aplicativo Trae (China)"],
        _ => ["托管网页登录", WEB, "粘贴凭据", "导入凭据JSON", "保留原始设备标识与成对密钥。仅国内账号；自动续期可能使官方客户端退出登录，续期凭据只能在已登录管理表单中导入。", "凭据JSON", "Trae 应用（国内）"],
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
        { "methodIndex": 2, "type": "api", "label": t[3], "instructions": t[4], "prompts": [
            { "key": "credentialJson", "type": "text", "label": t[5], "secret": true }
        ] }
    ] })
}
