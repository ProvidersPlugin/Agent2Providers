use agent2api_wasm_sdk::{json, Value};

pub fn methods(locale: &str) -> Value {
    let text = match locale {
        "en" => [
            "CodeArts chat, sign-in, account-specific models, usage and free rewards are implemented but not verified with a real account. Temporary access remains usable until expiry; automatic renewal requires the original OAuth material and a confirmed host enrollment.",
            "Paste CodeArts credential JSON",
            "Accepts a flat account or one credential/codearts_provider_credential object. Keep AK/SK/security_token, RFC3339 expiry, domain/user identity and the original PKCE/DPoP material. Pasting alone does not grant renewal; host enrollment is currently available only from the Windows desktop app.",
            "Complete credential JSON", "Account note", "CodeArts web sign-in",
            "Sign in on Huawei Cloud's official page. The gateway receives this flow's callback and enrolls its complete renewal material. In a remote panel, paste the browser's final loopback callback URL; follow any returned official continuation link, then paste the final URL. A temporary ticket fallback cannot renew automatically.",
        ],
        "zh-Hant" => [
            "CodeArts 對話、登入、帳號獨立模型、用量及免費福利已接通，尚未經真實帳號驗收。臨時憑據到期前可用，自動續期需原 OAuth 材料與主程式明確登記。",
            "貼上 CodeArts 憑據 JSON",
            "接受平鋪單帳號或唯一 credential/codearts_provider_credential 物件。保留 AK/SK/security_token、RFC3339 到期時間、域與使用者身份及原 PKCE/DPoP 材料。單純貼上不授予續期權；手工材料登記目前僅在 Windows 桌面應用提供。",
            "完整憑據 JSON", "備註名", "CodeArts 網頁登入",
            "在華為雲官方頁面登入。主程式接收本次回呼並登記完整續期材料；遠端面板可貼上瀏覽器最終本機回呼網址。若返回官方繼續連結，完成後再貼上最終網址。臨時 ticket 兜底無法自動續期。",
        ],
        "ja" => [
            "CodeArts の会話、ログイン、アカウント別モデル、使用量、無料特典は実装済みですが、実アカウントでは未検証です。一時認証情報は期限まで利用でき、自動更新には元の OAuth 情報とホストへの明示的登録が必要です。",
            "CodeArts 認証 JSON を貼り付け",
            "単一アカウントのフラットなオブジェクト、または credential/codearts_provider_credential を受け付けます。AK/SK/security_token、RFC3339 有効期限、ドメインとユーザー ID、元の PKCE/DPoP 情報を保持してください。貼り付けだけでは更新を許可しません。手動情報の登録は現在 Windows デスクトップアプリのみです。",
            "完全な認証 JSON", "メモ名", "CodeArts Web ログイン",
            "Huawei Cloud の公式ページでログインします。ゲートウェイが今回のコールバックを受信して更新情報を登録します。リモート画面ではブラウザーの最終ローカル URL を貼り付けてください。公式の続行リンクが表示された場合は完了後に最終 URL を再度貼り付けます。一時 ticket 方式は自動更新できません。",
        ],
        "ko" => [
            "CodeArts 대화, 로그인, 계정별 모델, 사용량과 무료 보상은 구현되었지만 실제 계정으로 검증되지 않았습니다. 임시 자격 증명은 만료 전까지 사용할 수 있으며 자동 갱신에는 원래 OAuth 자료와 호스트의 명시적 등록이 필요합니다.",
            "CodeArts 자격 증명 JSON 붙여넣기",
            "단일 계정의 일반 객체 또는 하나의 credential/codearts_provider_credential 객체를 받습니다. AK/SK/security_token, RFC3339 만료 시각, 도메인/사용자 ID와 원래 PKCE/DPoP 자료를 보존하세요. 붙여넣기만으로 갱신 권한을 주지 않습니다. 수동 자료 등록은 현재 Windows 데스크톱 앱에서만 제공됩니다.",
            "완전한 자격 증명 JSON", "계정 메모", "CodeArts 웹 로그인",
            "Huawei Cloud 공식 페이지에서 로그인합니다. 게이트웨이가 이번 콜백을 받고 갱신 자료를 등록합니다. 원격 화면에서는 브라우저의 최종 로컬 콜백 URL을 붙여넣으세요. 공식 계속 링크가 반환되면 진행을 완료한 후 최종 URL을 다시 붙여넣으세요. 임시 ticket 방식은 자동 갱신할 수 없습니다.",
        ],
        "pt-BR" => [
            "Conversa, login, modelos por conta, uso e benefícios gratuitos do CodeArts estão implementados, mas não foram validados com uma conta real. As credenciais temporárias funcionam até expirar; a renovação automática exige o material OAuth original e o cadastro explícito no host.",
            "Colar credenciais JSON do CodeArts",
            "Aceita uma conta em objeto simples ou um único objeto credential/codearts_provider_credential. Preserve AK/SK/security_token, validade RFC3339, IDs de domínio e usuário e o material PKCE/DPoP original. Colar não concede renovação; o cadastro manual está disponível apenas no aplicativo desktop Windows.",
            "Credenciais JSON completas", "Nome de observação", "Login web do CodeArts",
            "Entre na página oficial da Huawei Cloud. O gateway recebe o callback deste fluxo e cadastra o material de renovação. No painel remoto, cole a URL local final do navegador. Se surgir um link oficial para continuar, conclua-o e cole a URL final novamente. O fallback temporário por ticket não permite renovação automática.",
        ],
        _ => [
            "CodeArts 对话、登录、账号独立模型、用量与免费福利已接通，尚未经过真实账号验收。临时凭据到期前可用，自动续期需原 OAuth 材料和主程序明确登记。",
            "粘贴 CodeArts 凭据 JSON",
            "接受平铺单账号或唯一 credential/codearts_provider_credential 对象。保留 AK/SK/security_token、RFC3339 到期时间、域与用户身份及原 PKCE/DPoP 材料。仅粘贴不授予续期权；手工材料登记目前仅在 Windows 桌面应用提供。",
            "完整凭据 JSON", "备注名", "CodeArts 网页登录",
            "在华为云官方页面登录。主程序接收本次回调并登记完整续期材料；远程面板可粘贴浏览器最终本机回调地址。若返回官方继续链接，完成后再粘贴最终地址。临时 ticket 兜底无法自动续期。",
        ],
    };
    json!({ "instructions": text[0], "methods": [
        { "methodIndex": 0, "type": "api", "label": text[1], "instructions": text[2],
            "prompts": [
                { "key": "credentialJson", "type": "text", "label": text[3], "secret": true },
                { "key": "name", "type": "text", "label": text[4], "optional": true }
            ] },
        { "methodIndex": 1, "type": "oauth", "label": text[5], "prompts": [],
            "browserMode": "choice", "manualCallback": true, "callbackInput": "url", "validatesCallback": true,
            "instructions": text[6] }
    ] })
}
