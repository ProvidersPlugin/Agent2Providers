//! 原手工/JSON入口保留；方式0使用网关电脑的系统浏览器和原监听，不接受手填回调。

use agent2api_wasm_sdk::{json, Value};

pub const WEB: &str = "在运行网关的电脑上，用系统浏览器完成 Trae 官方授权，登录结果会自动接回，无需粘贴回调。网页和手机面板请使用凭据导入；本机加密登录态尚未接入。授权码和设备密钥由主程序持有，只兑换一次；尚未完成真实账号验收。";

fn texts(locale: &str) -> [&'static str; 7] {
    match locale {
        "en" => ["Managed web sign-in", "Complete official Trae authorization in the system browser on the gateway computer; the result returns automatically without pasting a callback. Use credential import in web and mobile panels. Encrypted local client credentials are not supported yet. The host keeps the authorization code and device keys for one exchange; real-account verification is pending.", "Paste credentials", "Import credentials JSON", "Keep the original device IDs and key pair. China accounts only. Automatic renewal may sign the official client out; refresh tokens are imported only through this authenticated form.", "Credentials JSON", "Trae app (China)"],
        "zh-Hant" => ["託管網頁登入", "在執行網關的電腦上，用系統瀏覽器完成 Trae 官方授權，登入結果會自動接回，無需貼上回呼。網頁和手機面板請使用憑證匯入；本機加密登入狀態尚未接入。授權碼和裝置金鑰由主程式持有，只交換一次；尚未完成真實帳號驗收。", "貼上憑證", "匯入憑證JSON", "保留原始裝置標識與成對金鑰。僅中國版帳號；自動續期可能使官方用戶端登出，續期憑證只能在已登入管理表單中匯入。", "憑證JSON", "Trae 應用（中國版）"],
        "ja" => ["管理されたウェブログイン", "ゲートウェイを実行するコンピューターのシステムブラウザーで Trae 公式認証を完了してください。結果は自動で戻り、コールバックの貼り付けは不要です。ウェブ・モバイル画面では認証情報の取り込みを使用してください。ローカルの暗号化されたログイン情報は未対応です。認可コードとデバイス鍵はホストが保持し、交換は1回だけです。実アカウントでの確認は未完了です。", "認証情報を貼り付け", "認証JSONを取り込む", "元のデバイスIDと鍵ペアを保持します。中国版のみ。自動更新により公式クライアントがログアウトする場合があります。更新トークンは認証済み管理フォームでのみ取り込めます。", "認証JSON", "Trae アプリ（中国版）"],
        "ko" => ["관리형 웹 로그인", "게이트웨이를 실행하는 컴퓨터의 시스템 브라우저에서 Trae 공식 인증을 완료하세요. 콜백을 붙여넣지 않아도 결과가 자동으로 돌아옵니다. 웹·모바일 화면에서는 인증 정보 가져오기를 사용하세요. 로컬의 암호화된 로그인 정보는 아직 지원하지 않습니다. 인증 코드와 기기 키는 호스트가 보관하며 한 번만 교환합니다. 실제 계정 검증은 아직 완료되지 않았습니다.", "인증 정보 붙여넣기", "인증 JSON 가져오기", "원래 기기 ID와 키 쌍을 유지하세요. 중국 계정만 지원합니다. 자동 갱신으로 공식 클라이언트가 로그아웃될 수 있으며 갱신 토큰은 인증된 관리 양식에서만 가져옵니다.", "인증 JSON", "Trae 앱(중국판)"],
        "pt-BR" => ["Login gerenciado pela web", "Conclua a autorização oficial do Trae no navegador do sistema no computador do gateway; o resultado retorna automaticamente, sem colar um retorno. Use a importação de credenciais nos painéis web e móvel. As credenciais locais criptografadas ainda não são suportadas. O host mantém o código e as chaves do dispositivo para uma única troca; a verificação com conta real está pendente.", "Colar credenciais", "Importar credenciais JSON", "Preserve os IDs e o par de chaves do dispositivo. Apenas contas da China. A renovação automática pode desconectar o cliente oficial; os tokens de renovação só são importados neste formulário autenticado.", "Credenciais JSON", "Aplicativo Trae (China)"],
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
        ], "browserMode": "external", "manualCallback": false, "callbackInput": "url" },
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
