//! 国内官方登录方式。网页登录只由本家桌面窗口捕获 Cookie，远程端保留凭据导入。

use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

use crate::{account, endpoints, gates};

fn text(locale: &str, items: [&'static str; 6]) -> &'static str {
    items[match locale {
        "en" => 1,
        "zh-Hant" => 2,
        "ja" => 3,
        "ko" => 4,
        "pt-BR" => 5,
        _ => 0,
    }]
}

fn instructions(locale: &str) -> &'static str {
    text(locale, [
        "添加前在 monkeycode-ai.com 校验账号；发送对话即授权本次固定远端任务，无需额外权限设置。镜像留空时使用官方默认环境，不购买或兑换。",
        "The account is verified on monkeycode-ai.com before saving. Sending a chat authorizes only that fixed remote task, with no extra permission setup. Leave the image ID empty for the official default environment. No purchases or exchanges are performed.",
        "儲存前在 monkeycode-ai.com 驗證帳號；傳送對話即授權本次固定遠端任務，無需額外權限設定。鏡像留空時使用官方預設環境，不購買或兌換。",
        "保存前に monkeycode-ai.com でアカウントを確認します。会話の送信はその固定リモートタスクだけを許可し、追加の権限設定は不要です。イメージ ID が空の場合は公式の既定環境を使います。購入や交換は行いません。",
        "저장 전에 monkeycode-ai.com에서 계정을 확인합니다. 대화를 보내면 해당 고정 원격 작업만 허용되며 별도 권한 설정은 필요하지 않습니다. 이미지 ID를 비우면 공식 기본 환경을 사용합니다. 구매나 교환은 하지 않습니다.",
        "A conta é validada em monkeycode-ai.com antes de salvar. Enviar uma conversa autoriza apenas essa tarefa remota fixa, sem configuração extra de permissões. Deixe o ID da imagem vazio para usar o ambiente oficial padrão. Nenhuma compra ou troca é realizada.",
    ])
}

fn web_hint(locale: &str) -> &'static str {
    text(locale, [
        "在桌面应用的独立窗口完成国内站官方登录，成功后自动接回并校验账号，无需粘贴回调；网页面板和手机可使用 Session／Cookie 或完整 JSON 导入。",
        "Complete official sign-in on the Chinese site in the app's separate desktop window. Credentials are captured and verified automatically, with no callback to paste. Web and mobile panels can import a Session / Cookie or complete JSON.",
        "請在桌面應用的獨立視窗完成國內站官方登入，成功後自動接回並驗證帳號，無需貼上回調；網頁面板和手機可匯入 Session／Cookie 或完整 JSON。",
        "デスクトップアプリの独立した画面で中国サイトの公式ログインを完了してください。認証情報を自動取得してアカウントを確認するため、コールバックの貼り付けは不要です。Web・モバイルでは Session／Cookie または完全な JSON を取り込めます。",
        "데스크톱 앱의 별도 창에서 중국 사이트의 공식 로그인을 완료하세요. 인증 정보를 자동으로 가져와 계정을 확인하므로 콜백을 붙여넣을 필요가 없습니다. 웹과 모바일에서는 Session / Cookie 또는 전체 JSON을 가져올 수 있습니다.",
        "Conclua o login oficial no site chinês na janela separada do aplicativo desktop. As credenciais são capturadas e verificadas automaticamente, sem colar um callback. Os painéis web e móvel podem importar Session / Cookie ou JSON completo.",
    ])
}

pub fn methods(locale: &str) -> Value {
    let manual = text(
        locale,
        [
            "粘贴 Session／Cookie",
            "Paste Session / Cookie",
            "貼上 Session／Cookie",
            "Session／Cookie を貼り付け",
            "Session / Cookie 붙여넣기",
            "Colar Session / Cookie",
        ],
    );
    let web = text(
        locale,
        [
            "MonkeyCode 网页登录",
            "Sign in to MonkeyCode",
            "MonkeyCode 網頁登入",
            "MonkeyCode にログイン",
            "MonkeyCode 로그인",
            "Entrar no MonkeyCode",
        ],
    );
    let name = text(locale, ["备注名", "Note", "備註", "メモ", "메모", "Nota"]);
    let image = text(
        locale,
        [
            "执行环境镜像编号（可选）",
            "VM image ID (optional)",
            "鏡像編號（選填）",
            "イメージ ID（任意）",
            "이미지 ID (선택)",
            "ID da imagem (opcional)",
        ],
    );
    let json_label = text(
        locale,
        [
            "导入完整 JSON",
            "Import complete JSON",
            "匯入完整 JSON",
            "完全な JSON を取り込む",
            "전체 JSON 가져오기",
            "Importar JSON completo",
        ],
    );
    json!({ "instructions": instructions(locale), "readiness": gates::status(), "methods": [
        { "methodIndex": 0, "type": "api", "label": manual, "instructions": instructions(locale), "prompts": [
            { "key": "session", "type": "text", "label": "Session / Cookie", "secret": true, "placeholder": "monkeycode_ai_session=…" },
            { "key": "name", "type": "text", "label": name, "optional": true },
            { "key": "imageId", "type": "text", "label": image, "optional": true }
        ] },
        { "methodIndex": 1, "type": "oauth", "label": web, "instructions": web_hint(locale), "prompts": [],
            "browserMode": "choice", "embeddedOnly": true, "desktopOnly": true, "manualCallback": false,
            "validatesCallback": true },
        { "methodIndex": 2, "type": "api", "label": json_label, "instructions": instructions(locale), "prompts": [
            { "key": "credentialJson", "type": "text", "label": json_label, "secret": true },
            { "key": "name", "type": "text", "label": name, "optional": true }
        ] }
    ] })
}

pub fn start(params: &Value) -> Result<Value> {
    let options = &params["payload"];
    if options
        .get("methodIndex")
        .is_some_and(|index| index.as_u64() != Some(1))
        || options.get("mode").is_some_and(|mode| mode != "embedded")
        || options.get("inputs").is_some_and(|inputs| {
            !inputs.is_object() || inputs.as_object().is_some_and(|fields| !fields.is_empty())
        })
    {
        return Err(gates::web_login());
    }
    let locale = options["locale"].as_str().unwrap_or("zh-CN");
    let mut started = sdk::cookie_login::start(
        endpoints::ID,
        endpoints::ORIGIN,
        &["monkeycode-ai.com", "baizhi.cloud"],
    )?;
    let flow = started["flow"]
        .as_str()
        .ok_or_else(Error::unsupported)?
        .to_string();
    sdk::session_put(
        &flow,
        json!({ "provider": endpoints::ID, "state": "waiting", "locale": locale }),
        300_000,
    )?;
    started["instructions"] = json!(web_hint(locale));
    Ok(started)
}

pub fn callback(params: &Value) -> Result<Value> {
    sdk::cookie_login::callback(endpoints::ID, params, account::normalize).map(|mut result| {
        if result["account"]["secret"].is_object() {
            result["account"]["secret"]["source"] = json!("web");
            result["account"]["public"] = account::view(&result["account"]["secret"]);
        }
        result
    })
}

pub fn poll(params: &Value) -> Result<Value> {
    let flow = params["flow"]
        .as_str()
        .filter(|flow| flow.starts_with("cookie_login_") && flow.len() <= 128)
        .ok_or_else(|| Error::http(400, "MonkeyCode 登录流程无效"))?;
    let pending = sdk::session_get(flow)?;
    if pending["provider"] != endpoints::ID || pending["state"] != "waiting" {
        return Ok(
            json!({ "state": "failed", "message": "MonkeyCode 登录已取消或过期，请重新发起" }),
        );
    }
    Ok(
        json!({ "state": "pending", "message": web_hint(pending["locale"].as_str().unwrap_or("zh-CN")) }),
    )
}
