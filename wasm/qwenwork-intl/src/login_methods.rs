//! 国际设备授权和手工文本入口，方法索引与地区身份固定，说明跟随本次界面语言。
use agent2api_wasm_sdk::{json, Value};

fn text(locale: &str) -> [&'static str; 11] {
    match locale {
        "en" => ["Browser device authorization", "Authorize your QwenWork International account on the official page to add the account automatically.", "Enter device credentials", "Import credentials JSON", "Credentials JSON", "Device ID (optional)", "User ID (optional)", "Refresh token (optional)", "Expires at (optional)", "Enter an Access Token to add the account; include a Refresh Token to enable automatic renewal.", "Select or paste a QwenWork International credential file to import the account; automatic renewal is supported when complete sign-in details are included."],
        "ja" => ["ブラウザーでデバイス認証", "公式ページで千問辦公（国際版）のアカウントを認証すると自動で追加されます。", "デバイス認証情報を入力", "認証情報JSONを取り込む", "認証情報JSON", "デバイスID（任意）", "ユーザーID（任意）", "更新トークン（任意）", "有効期限（任意）", "Access Token を入力してアカウントを追加できます。Refresh Token を入力すると自動更新に対応します。", "千問辦公（国際版）の認証ファイルを選択または貼り付けてアカウントを取り込めます。完全なログイン情報が含まれていれば自動更新に対応します。"],
        "ko" => ["브라우저 기기 인증", "공식 페이지에서 QwenWork 국제판 계정을 인증하면 자동으로 추가됩니다.", "기기 자격 증명 입력", "자격 증명 JSON 가져오기", "자격 증명 JSON", "기기 ID (선택)", "사용자 ID (선택)", "갱신 토큰 (선택)", "만료 시각 (선택)", "Access Token을 입력해 계정을 추가할 수 있으며, Refresh Token을 함께 입력하면 자동 갱신을 지원합니다.", "QwenWork 국제판 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있으며, 전체 로그인 정보가 포함되어 있으면 자동 갱신을 지원합니다."],
        "pt-BR" => ["Autorizar dispositivo no navegador", "Autorize a conta QwenWork International na página oficial para adicionar a conta automaticamente.", "Inserir credenciais do dispositivo", "Importar credenciais JSON", "Credenciais JSON", "ID do dispositivo (opcional)", "ID do usuário (opcional)", "Token de renovação (opcional)", "Expiração (opcional)", "Informe o Access Token para adicionar a conta; inclua o Refresh Token para ativar a renovação automática.", "Selecione ou cole um arquivo de credenciais do QwenWork International para importar a conta; a renovação automática é suportada quando os dados completos de login estão incluídos."],
        "zh-Hant" => ["網頁裝置授權", "在官方頁面授權千問辦公國際版帳號即可自動新增帳號。", "手動填寫裝置憑證", "匯入憑證JSON", "憑證JSON", "裝置編號（選填）", "使用者編號（選填）", "更新令牌（選填）", "到期時刻（選填）", "填寫 Access Token 即可新增帳號；填寫 Refresh Token 後支援自動續期。", "選擇或貼上千問辦公國際版憑據檔案即可匯入帳號；包含完整登入資訊時支援自動續期。"],
        _ => ["网页设备授权", "在官方页面授权千问办公国际版账号即可自动添加账号。", "手工填写设备凭据", "导入凭据JSON", "凭据JSON", "设备编号（选填）", "用户编号（选填）", "刷新令牌（选填）", "到期时刻（选填）", "填写 Access Token 即可添加账号；填写 Refresh Token 后支持自动续期。", "选择或粘贴千问办公国际版凭据文件即可导入账号；包含完整登录信息时支持自动续期。"],
    }
}
pub fn instructions(locale: &str) -> &'static str {
    text(locale)[1]
}
pub fn progress(locale: &str, kind: &str) -> &'static str {
    let texts = match locale {
        "en" => ["Device authorization was canceled or expired; start again. No late account was saved.", "The previous authorization result is unknown; start again. Device credentials were not requested twice.", "The authorization service is temporarily unavailable; waiting to retry."],
        "ja" => ["認証が取り消されたか期限切れです。遅延したアカウントは保存せず、再開始してください。", "前回の認証結果が不明です。認証情報を再取得せず、認証を再開始してください。", "認証サービスに一時的に接続できません。再試行を待っています。"],
        "ko" => ["인증이 취소되었거나 만료되었습니다. 늦은 계정은 저장하지 않았으니 다시 시작하세요.", "이전 인증 결과를 확인할 수 없습니다. 자격 증명을 중복 요청하지 않았으니 다시 인증하세요.", "인증 서비스에 잠시 연결할 수 없어 재시도를 기다립니다."],
        "pt-BR" => ["A autorização foi cancelada ou expirou. Inicie novamente; nenhuma conta atrasada foi salva.", "O resultado anterior é desconhecido. Inicie novamente; as credenciais não foram solicitadas duas vezes.", "O serviço de autorização está temporariamente indisponível; aguardando nova tentativa."],
        "zh-Hant" => ["裝置授權已取消或過期，請重新發起；未保存遲到的帳號。", "上輪授權結果未確認，請重新發起；未重複領取裝置憑證。", "裝置授權服務暫時不可達，正在等待重試。"],
        _ => ["设备授权已取消或过期，请重新发起；未保存迟到的账号。", "上轮授权结果未确认，请重新发起；未重复领取设备凭据。", "设备授权服务暂时不可达，正在等待重试。"],
    };
    texts[match kind {
        "unknown" => 1,
        "retry" => 2,
        _ => 0,
    }]
}
pub fn methods(locale: &str) -> Value {
    let text = text(locale);
    json!({ "methods": [
        { "methodIndex": 0, "type": "oauth", "label": text[0], "instructions": text[1], "prompts": [],
            "browserMode": "choice", "manualCallback": false, "callbackInput": "url" },
        { "methodIndex": 1, "type": "api", "label": text[2], "instructions": text[9], "prompts": [
            { "key": "accessToken", "type": "text", "label": "Access Token", "secret": true },
            { "key": "refreshToken", "type": "text", "label": text[7], "secret": true, "optional": true },
            { "key": "userId", "type": "text", "label": text[6], "optional": true },
            { "key": "machineId", "type": "text", "label": text[5], "optional": true },
            { "key": "expiresAt", "type": "text", "label": text[8], "optional": true }
        ] },
        { "methodIndex": 2, "type": "api", "label": text[3], "instructions": text[10], "prompts": [
            { "key": "credentialJson", "type": "text", "label": text[4], "secret": true }
        ] }
    ] })
}
