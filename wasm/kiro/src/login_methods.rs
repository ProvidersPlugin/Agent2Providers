//! 保持原生三种入口及新增独立JSON导入项；未就绪能力显示真实原因而不是消失。

use agent2api_wasm_sdk::{json, Value};

pub const WEB: &str = "本次网页登录由主程序托管回调与一次性授权步骤。门户选择AWS后会显示第二步授权，请按当前步骤完成；远程部署可粘贴当前完整回调地址。过期或失败请重新发起，不会重发授权码。";
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
        { "methodIndex": 2, "type": "api", "label": "Kiro API key", "instructions": "粘贴ksk_开头的API key；此类凭据不自动续期。",
            "prompts": [{ "key": "apiKey", "type": "text", "label": "API key", "secret": true, "placeholder": "ksk_…" },
                { "key": "name", "type": "text", "label": "备注名", "optional": true }] },
        { "methodIndex": 3, "type": "api", "label": "导入凭据JSON",
            "instructions": "接受手工选择并粘贴的Kiro OAuth/API key JSON；不扫描客户端文件。外部身份凭据需保留刷新令牌、客户端编号和官方换证地址，才能自动续期。",
            "prompts": [{ "key": "credentialJson", "type": "text", "label": "凭据JSON", "secret": true },
                { "key": "name", "type": "text", "label": "备注名", "optional": true }] }
    ] });
    value["methods"][1]["instructions"] = json!(match locale {
        "en" => "Links the official Kiro CLI or IDE login on the gateway computer. Local renewal and writing back rotated credentials require separate permission in plugin settings; otherwise, sign in again in the official client.",
        "ja" => "ゲートウェイのコンピューター上の公式 Kiro CLI または IDE のログインを参照します。本機の更新と認証情報の書き戻しにはプラグイン設定で別途許可が必要です。許可しない場合は公式クライアントで再ログインしてください。",
        "ko" => "게이트웨이 컴퓨터의 공식 Kiro CLI 또는 IDE 로그인에 연결합니다. 로컬 갱신과 새 인증 정보 저장은 플러그인 설정에서 별도 허용해야 합니다. 허용하지 않으면 공식 클라이언트에 다시 로그인하세요.",
        "pt-BR" => "Vincula ao login oficial do Kiro CLI ou IDE no computador do gateway. Renovar e gravar credenciais atualizadas exige permissão separada nas configurações do plugin; sem ela, entre novamente no cliente oficial.",
        "zh-Hant" => "關聯閘道電腦上的官方 Kiro CLI／IDE 登入。本機續期及寫回新憑據需在插件設定中另外授權；未授權時請在官方用戶端重新登入。",
        _ => "关联网关电脑上的官方 Kiro CLI／IDE 登录。本机续期及写回新凭据需在插件设置中另外授权；未授权时请在官方客户端重新登录。",
    });
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
         "The gateway manages callbacks and one-time authorization. Choosing AWS in the portal opens a second authorization step. Complete the current step; for remote deployments, paste its full callback URL. Restart after expiry or failure; authorization codes are never resent.",
         "ゲートウェイがコールバックと一度限りの認証を管理します。ポータルで AWS を選ぶと第2段階が表示されます。現在の手順を完了してください。リモート接続では完全なコールバック URL を貼り付けます。期限切れや失敗時はやり直してください。認証コードは再送しません。",
         "게이트웨이가 콜백과 일회용 인증을 관리합니다. 포털에서 AWS를 선택하면 두 번째 인증 단계가 나타납니다. 현재 단계를 완료하세요. 원격 연결에서는 전체 콜백 주소를 붙여넣을 수 있습니다. 만료되거나 실패하면 다시 시작하세요. 인증 코드를 재전송하지 않습니다.",
         "O gateway gerencia os retornos e a autorização de uso único. Ao escolher AWS no portal, aparece uma segunda etapa. Conclua a etapa atual; em conexões remotas, cole a URL completa de retorno. Reinicie se expirar ou falhar; códigos não são reenviados.",
         "本次網頁登入由閘道託管回呼與一次性授權。入口網站選擇 AWS 後會顯示第二步，請完成目前步驟；遠端部署可貼上完整回呼網址。過期或失敗請重新發起，不會重送授權碼。"],
        ["登录方式", "Sign-in method", "ログイン方法", "로그인 방식", "Método de login", "登入方式"],
        ["Kiro登录页（Google / GitHub / AWS）", "Kiro portal (Google / GitHub / AWS)", "Kiro ログインページ（Google / GitHub / AWS）", "Kiro 로그인 페이지 (Google / GitHub / AWS)", "Portal do Kiro (Google / GitHub / AWS)", "Kiro 登入頁（Google / GitHub / AWS）"],
        ["组织登录地址", "Organization sign-in URL", "組織ログイン URL", "조직 로그인 주소", "URL de login da organização", "組織登入網址"],
        ["AWS目录地区", "AWS directory region", "AWS ディレクトリのリージョン", "AWS 디렉터리 리전", "Região do diretório AWS", "AWS 目錄區域"],
        ["粘贴ksk_开头的API key；此类凭据不自动续期。", "Paste an API key starting with ksk_. API keys are not automatically refreshed.", "ksk_ で始まる API キーを貼り付けてください。API キーは自動更新しません。", "ksk_로 시작하는 API 키를 붙여넣으세요. API 키는 자동 갱신하지 않습니다.", "Cole uma chave API que começa com ksk_. Chaves API não são renovadas automaticamente.", "貼上 ksk_ 開頭的 API key；此類憑據不自動續期。"],
        ["备注名", "Display name", "表示名", "표시 이름", "Nome de exibição", "備註名稱"],
        ["导入凭据JSON", "Import credentials JSON", "認証情報 JSON を取り込む", "인증 정보 JSON 가져오기", "Importar JSON de credenciais", "匯入憑據 JSON"],
        ["接受手工选择并粘贴的Kiro OAuth/API key JSON；不扫描客户端文件。外部身份凭据需保留刷新令牌、客户端编号和官方换证地址，才能自动续期。",
         "Paste a Kiro OAuth or API key JSON that you selected yourself. Client files are not scanned. External identity credentials require a refresh token, client ID and official token endpoint for automatic renewal.",
         "選択した Kiro OAuth または API キー JSON を貼り付けてください。クライアントのファイルは検索しません。外部 ID 認証の自動更新には、更新トークン、クライアント ID と公式のトークン URL が必要です。",
         "직접 선택한 Kiro OAuth 또는 API 키 JSON을 붙여넣으세요. 클라이언트 파일을 검색하지 않습니다. 외부 인증 자동 갱신에는 갱신 토큰, 클라이언트 ID와 공식 토큰 주소가 필요합니다.",
         "Cole um JSON de OAuth ou chave API do Kiro selecionado por você. Não há busca em arquivos do cliente. A renovação de credenciais externas exige refresh token, ID do cliente e endpoint oficial de tokens.",
         "接受手動選取並貼上的 Kiro OAuth/API key JSON；不掃描用戶端檔案。外部身分憑據需保留刷新權杖、用戶端編號與官方換證位址，才能自動續期。"],
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
