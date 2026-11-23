//! 国际版动态登录文案，语言只影响展示，不改变凭据、流程或授权来源。
pub fn locale(value: &str) -> &'static str {
    match value {
        "en" => "en",
        "ja" => "ja",
        "ko" => "ko",
        "pt-BR" => "pt-BR",
        "zh-Hant" => "zh-Hant",
        _ => "zh-CN",
    }
}

pub fn text(locale: &str, key: &str) -> String {
    let index = match locale {
        "en" => 1,
        "zh-Hant" => 2,
        "ja" => 3,
        "ko" => 4,
        "pt-BR" => 5,
        _ => 0,
    };
    let lines = match key {
        "authorize" => [
            "请在打开的页面完成 Z.AI 国际版授权，完成后会自动换取编码套餐密钥，无需粘贴回调地址。",
            "Authorize your international Z.AI account in the opened page. The coding key is then resolved automatically; no callback URL needs to be pasted.",
            "請在開啟的頁面完成 Z.AI 國際版授權，完成後會自動取得編碼套餐金鑰，無需貼上回呼網址。",
            "開いたページで国際版 Z.AI の認証を完了してください。コーディング用キーを自動取得します。コールバック URL の貼り付けは不要です。",
            "열린 페이지에서 국제판 Z.AI 계정을 인증하세요. 코딩 키를 자동으로 가져오므로 콜백 주소를 붙여 넣을 필요가 없습니다.",
            "Autorize sua conta internacional Z.AI na página aberta. A chave de programação será obtida automaticamente; não é preciso colar a URL de retorno.",
        ],
        "resolve_key" => [
            "网页授权已完成，正在核对机构、项目并获取完整编码密钥，请等待。",
            "Browser authorization is complete. Checking the organization and project and resolving the full coding key; please wait.",
            "網頁授權已完成，正在核對機構、專案並取得完整編碼金鑰，請稍候。",
            "ブラウザーでの認証が完了しました。組織とプロジェクトを確認し、完全なコーディング用キーを取得しています。",
            "웹 인증이 완료되었습니다. 조직과 프로젝트를 확인하고 전체 코딩 키를 가져오는 중입니다.",
            "A autorização no navegador foi concluída. Verificando a organização e o projeto e obtendo a chave completa; aguarde.",
        ],
        "cancelled" => [
            "ZCode 国际版登录已取消或过期，请重新发起。", "The international ZCode sign-in was cancelled or expired. Start again.",
            "ZCode 國際版登入已取消或過期，請重新發起。", "国際版 ZCode のログインはキャンセルされたか期限切れです。やり直してください。",
            "국제판 ZCode 로그인이 취소되었거나 만료되었습니다. 다시 시작하세요.", "O login internacional do ZCode foi cancelado ou expirou. Inicie novamente.",
        ],
        "expired" => [
            "ZCode 国际版登录已过期，请重新发起。", "The international ZCode sign-in expired. Start again.",
            "ZCode 國際版登入已過期，請重新發起。", "国際版 ZCode のログインが期限切れです。やり直してください。",
            "국제판 ZCode 로그인이 만료되었습니다. 다시 시작하세요.", "O login internacional do ZCode expirou. Inicie novamente.",
        ],
        "manual" => [
            "编码套餐请填写 Access Token / API Key，套餐池请填写 ZCode JWT。",
            "Use an Access Token / API Key for Coding Plan or a ZCode JWT for Start Plan.",
            "編碼套餐請填寫 Access Token / API Key，套餐池請填寫 ZCode JWT。",
            "Coding Plan は Access Token / API Key、Start Plan は ZCode JWT を入力してください。",
            "Coding Plan은 액세스 토큰/API 키, Start Plan은 ZCode JWT를 입력하세요.",
            "Use um token de acesso/chave de API para Coding Plan ou um JWT do ZCode para Start Plan.",
        ],
        "user" => ["用户标识（选填）", "User ID (optional)", "使用者標識（選填）", "ユーザー ID（任意）", "사용자 ID(선택)", "ID do usuário (opcional)"],
        "device" => ["设备 UUID（选填）", "Device UUID (optional)", "裝置 UUID（選填）", "デバイス UUID（任意）", "장치 UUID(선택)", "UUID do dispositivo (opcional)"],
        "plan" => ["使用套餐", "Plan channel", "使用套餐", "プラン経路", "요금제 채널", "Canal do plano"],
        "coding" => ["编码套餐（Coding Plan）", "Coding Plan", "編碼套餐（Coding Plan）", "Coding Plan", "Coding Plan", "Coding Plan"],
        "start" => ["套餐池（Start Plan）", "Start Plan", "套餐池（Start Plan）", "Start Plan", "Start Plan", "Start Plan"],
        _ => ["", "", "", "", "", ""],
    };
    lines[index].to_string()
}
