//! 国际设备授权和手工文本入口，方法索引与地区身份固定，说明跟随本次界面语言。
use agent2api_wasm_sdk::{json, Value};

fn text(locale: &str) -> [&'static str; 9] {
    match locale {
        "en" => ["Browser device authorization", "Authorize your QwenWork International account on the official page; it is saved automatically without a local callback. Import access/refresh credentials or a complete JSON record from the international client. User and device IDs are checked, and a missing device ID is generated once and saved. Compatibility has not been verified with a real international account. The China and enterprise pools are not imported here; the client files are not read or changed.", "Enter device credentials", "Import credentials JSON", "Credentials JSON", "Device ID (optional; preserve the original value)", "User ID (optional; read from account information if absent)", "Refresh token (optional; access-only records cannot renew)", "Expires at (optional; Unix seconds, milliseconds or an ISO date)"],
        "ja" => ["ブラウザーでデバイス認証", "公式ページで千問办公国際版を認証すると、自動保存されます。ローカルコールバックは不要です。国際版のアクセス・更新認証情報または完全なJSONを取り込めます。ユーザーとデバイスIDを照合し、IDがなければ一度生成して保存します。実アカウントでは未検証です。中国版・企業プールは取り込まず、公式クライアントのファイルは読み書きしません。", "デバイス認証情報を入力", "認証情報JSONを取り込む", "認証情報JSON", "デバイスID（任意・元の値を保持）", "ユーザーID（任意・未入力時は照会）", "更新トークン（任意・アクセスのみでは更新不可）", "有効期限（任意・秒、ミリ秒またはISO日時）"],
        "ko" => ["브라우저 기기 인증", "공식 페이지에서 QwenWork 국제판을 인증하면 로컬 콜백 없이 자동 저장됩니다. 국제판의 액세스/갱신 자격 증명이나 전체 JSON을 가져올 수 있습니다. 사용자와 기기 ID를 확인하며 없으면 한 번 생성해 저장합니다. 실제 국제 계정에서는 검증되지 않았습니다. 중국판이나 기업 풀을 가져오지 않으며 공식 클라이언트 파일을 읽거나 변경하지 않습니다.", "기기 자격 증명 입력", "자격 증명 JSON 가져오기", "자격 증명 JSON", "기기 ID (선택; 원래 값 유지)", "사용자 ID (선택; 없으면 계정 조회)", "갱신 토큰 (선택; 액세스 전용은 갱신 불가)", "만료 시각 (선택; 초, 밀리초 또는 ISO 날짜)"],
        "pt-BR" => ["Autorizar dispositivo no navegador", "Autorize a conta QwenWork International na página oficial; ela será salva automaticamente sem callback local. Importe as credenciais de acesso/renovação ou o JSON completo da edição internacional. Os IDs de usuário e dispositivo são conferidos; um ID ausente é gerado uma vez e salvo. Não foi validado com conta internacional real. Não importa contas da China ou do pool empresarial, nem lê ou altera arquivos do cliente oficial.", "Inserir credenciais do dispositivo", "Importar credenciais JSON", "Credenciais JSON", "ID do dispositivo (opcional; preserve o original)", "ID do usuário (opcional; consultar se ausente)", "Token de renovação (opcional; só acesso não permite renovar)", "Expiração (opcional; segundos, milissegundos ou data ISO)"],
        "zh-Hant" => ["網頁裝置授權", "請在官方頁面授權千問辦公國際版帳號，完成後會自動保存，不需要本機回呼。也可匯入國際版的訪問／更新憑證或完整JSON。使用者與裝置編號會核對；缺少裝置編號時僅生成並保存一次。尚未使用真實國際帳號驗收；不匯入國內或企業額度池，不讀取或修改官方客戶端檔案。", "手動填寫裝置憑證", "匯入憑證JSON", "憑證JSON", "裝置編號（選填，請保留原值）", "使用者編號（選填，缺省查詢帳號資料）", "更新令牌（選填，僅訪問令牌不能自動續期）", "到期時刻（選填，秒／毫秒時間戳或ISO日期）"],
        _ => ["网页设备授权", "请在官方页面授权千问办公国际版账号，完成后会自动保存，不需要本机回调。也可导入国际版的访问／刷新凭据或完整JSON。用户和设备编号会核对；缺少设备编号时仅生成并保存一次。尚未使用真实国际账号验收；不导入国内或企业额度池，不读取或修改官方客户端文件。", "手工填写设备凭据", "导入凭据JSON", "凭据JSON", "设备编号（选填，请保留原值）", "用户编号（选填，缺省查询账号资料）", "刷新令牌（选填，仅访问令牌不能自动续期）", "到期时刻（选填，秒／毫秒时间戳或ISO日期）"],
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
    json!({ "instructions": text[1], "methods": [
        { "methodIndex": 0, "type": "oauth", "label": text[0], "instructions": text[1], "prompts": [],
            "browserMode": "external", "manualCallback": false, "callbackInput": "url" },
        { "methodIndex": 1, "type": "api", "label": text[2], "instructions": text[1], "prompts": [
            { "key": "accessToken", "type": "text", "label": "Access Token", "secret": true },
            { "key": "refreshToken", "type": "text", "label": text[7], "secret": true, "optional": true },
            { "key": "userId", "type": "text", "label": text[6], "optional": true },
            { "key": "machineId", "type": "text", "label": text[5], "optional": true },
            { "key": "expiresAt", "type": "text", "label": text[8], "optional": true }
        ] },
        { "methodIndex": 2, "type": "api", "label": text[3], "instructions": text[1], "prompts": [
            { "key": "credentialJson", "type": "text", "label": text[4], "secret": true }
        ] }
    ] })
}
