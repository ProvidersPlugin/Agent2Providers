//! 登录方法与三端提交保持一致，本机快照走添加账号，不创建伪OAuth流程。

use agent2api_wasm_sdk::{json, Result, Value};

use crate::{credentials, errors, gates};

pub fn methods(locale: &str) -> Value {
    let mut value = json!({ "instructions": gates::SUMMARY, "readiness": gates::status(), "methods": [
        { "methodIndex": 0, "type": "api", "label": "粘贴完整 Cookie 快照",
            "instructions": "必须含 sessionid 或 sessionid_ss，以及原登录客户端的 deviceId；仅 Cookie 无法形成完整的豆包工作请求上下文。添加前会在线核对账号，不读取其他本机登录态。",
            "prompts": [
                { "key": "cookie", "type": "text", "label": "完整 Cookie 或 Cookie JSON", "secret": true,
                    "placeholder": "sessionid=…; sessionid_ss=…" },
                { "key": "deviceId", "type": "text", "label": "deviceId", "secret": true, "optional": true,
                    "placeholder": "完整 JSON 已包含时无需填写" },
                { "key": "webId", "type": "text", "label": "webId", "secret": true, "optional": true },
                { "key": "clientVersion", "type": "text", "label": "客户端版本", "optional": true },
                { "key": "name", "type": "text", "label": "备注名", "optional": true }
            ] },
        { "methodIndex": 1, "type": "api", "label": "导入文本 JSON / Cookie 快照",
            "instructions": "粘贴含 Cookie 和原登录设备 deviceId 的单账号 JSON；已有用户身份会与在线结果核对，不自动换人。缺少设备信息时请重新导入本机登录态。",
            "prompts": [
                { "key": "credentialText", "type": "text", "label": "快照文本", "secret": true },
                { "key": "name", "type": "text", "label": "备注名", "optional": true }
            ] },
        { "methodIndex": 2, "type": "oauth", "label": "豆包工作网页登录", "prompts": [],
            "browserMode": "plugin", "embeddedOnly": true, "manualCallback": true, "callbackInput": "code", "validatesCallback": true,
            "instructions": "在官方页面完成登录，桌面窗口读取本次 Cookie、设备和客户端上下文并核对账号；网页面板可粘贴完整 Cookie JSON。" },
        agent2api_wasm_sdk::login::desktop_method(3, "豆包工作 / Doubao Work", locale)
    ] });
    translate(&mut value, locale);
    value
}

pub fn start(params: &Value) -> Result<Value> {
    let index = params["payload"]["methodIndex"].as_u64();
    if index.is_some_and(|index| index != 2) || params["method"] == "desktop" {
        return Err(errors::invalid(
            "本机或凭据导入请使用添加账号入口，不发起网页登录",
        ));
    }
    agent2api_wasm_sdk::cookie_login::start(
        "doubaowork",
        "https://www.doubao.com",
        &["doubao.com", "feishu.cn", "larksuite.com"],
    )
}
pub fn poll(params: &Value) -> Result<Value> {
    let flow = params["flow"]
        .as_str()
        .filter(|flow| flow.starts_with("cookie_login_") && flow.len() <= 128)
        .ok_or_else(|| errors::invalid("登录流程缺失或无效"))?;
    let value = agent2api_wasm_sdk::session_get(flow)?;
    if value["provider"] != "doubaowork" || value["state"] != "waiting" {
        return Err(errors::invalid("登录已经过期或取消"));
    }
    Ok(json!({"state":"pending"}))
}

fn translate(value: &mut Value, locale: &str) {
    let index = match locale {
        "zh-Hant" => 0,
        "en" => 1,
        "ja" => 2,
        "ko" => 3,
        "pt-BR" => 4,
        _ => return,
    };
    match value {
        Value::String(text) => {
            if let Some((_, translated)) = LOGIN_TEXT
                .iter()
                .find(|(original, _)| *original == text.as_str())
            {
                *text = translated[index].to_string();
            }
        }
        Value::Array(rows) => rows.iter_mut().for_each(|row| translate(row, locale)),
        Value::Object(fields) => fields
            .values_mut()
            .for_each(|field| translate(field, locale)),
        _ => {}
    }
}

const LOGIN_TEXT: &[(&str, [&str; 5])] = &[
    (gates::SUMMARY, [
        "豆包工作已接通對話、思考、用戶端工具橋、圖片輸入和生圖、網頁登入、本機快照匯入、模型目錄、額度、每日簽到與已完成任務領獎；未執行真實帳號驗收。",
        "Doubao Work implements chat, reasoning, client-side tool bridging, image input and generation, browser sign-in, local snapshot import, model catalogs, quotas, daily check-in and completed-task rewards. Real-account acceptance is pending.",
        "Doubao Workは対話、推論、クライアント側ツール連携、画像入力と生成、ウェブログイン、ローカルスナップショットの取り込み、モデル一覧、利用枠、日次チェックインと完了タスクの報酬に対応する実装があります。実アカウントでの検証は未実施です。",
        "Doubao Work의 대화, 추론, 클라이언트 도구 연동, 이미지 입력과 생성, 웹 로그인, 로컬 스냅샷 가져오기, 모델 목록, 사용 한도, 일일 체크인과 완료된 작업 보상이 구현되어 있습니다. 실제 계정 검증은 하지 않았습니다.",
        "O Doubao Work implementa conversa, raciocínio, integração de ferramentas do cliente, entrada e geração de imagens, login web, importação de estado local, catálogo, cotas, check-in diário e recompensas de tarefas concluídas. Ainda sem validação com conta real.",
    ]),
    ("粘贴完整 Cookie 快照", ["貼上完整 Cookie 快照", "Paste a complete cookie snapshot", "Cookieスナップショットを貼り付け", "전체 Cookie 스냅샷 붙여넣기", "Colar snapshot completo de cookies"]),
    ("必须含 sessionid 或 sessionid_ss，以及原登录客户端的 deviceId；仅 Cookie 无法形成完整的豆包工作请求上下文。添加前会在线核对账号，不读取其他本机登录态。", [
        "必須含 sessionid 或 sessionid_ss，以及原登入用戶端的 deviceId；僅 Cookie 無法形成完整的豆包工作請求。新增前會線上核對帳號，不讀取其他本機登入狀態。",
        "Include sessionid or sessionid_ss and the original sign-in client's deviceId. Cookies alone are insufficient. The account is checked online before saving; no other local sign-in state is read.",
        "sessionidまたはsessionid_ssと、ログインした元クライアントのdeviceIdが必要です。Cookieだけでは不十分です。保存前にオンラインでアカウントを確認し、他のローカルログイン状態は読みません。",
        "sessionid 또는 sessionid_ss와 로그인한 원래 클라이언트의 deviceId가 필요합니다. Cookie만으로는 부족합니다. 저장 전에 온라인으로 계정을 확인하며 다른 로컬 로그인 상태는 읽지 않습니다.",
        "Inclua sessionid ou sessionid_ss e o deviceId do cliente original de login. Só os cookies não bastam. A conta é conferida online antes de salvar; nenhum outro estado local é lido.",
    ]),
    ("完整 Cookie 或 Cookie JSON", ["完整 Cookie 或 Cookie JSON", "Complete cookie header or JSON", "完全なCookieまたはCookie JSON", "전체 Cookie 또는 Cookie JSON", "Cookie completo ou JSON de cookies"]),
    ("完整 JSON 已包含时无需填写", ["完整 JSON 已包含時無需填寫", "Optional if already included in the JSON", "JSONに含まれていれば入力不要", "JSON에 포함되어 있으면 입력하지 않아도 됩니다", "Opcional se já estiver no JSON"]),
    ("客户端版本", ["用戶端版本", "Client version", "クライアントのバージョン", "클라이언트 버전", "Versão do cliente"]),
    ("备注名", ["備註名稱", "Account label", "表示名", "계정 표시 이름", "Nome da conta"]),
    ("导入文本 JSON / Cookie 快照", ["匯入文字 JSON / Cookie 快照", "Import JSON / cookie snapshot", "JSON / Cookieスナップショットを取り込み", "JSON / Cookie 스냅샷 가져오기", "Importar JSON / snapshot de cookies"]),
    ("粘贴含 Cookie 和原登录设备 deviceId 的单账号 JSON；已有用户身份会与在线结果核对，不自动换人。缺少设备信息时请重新导入本机登录态。", [
        "貼上含 Cookie 和原登入裝置 deviceId 的單帳號 JSON；既有使用者身分會與線上結果核對，不自動換人。缺少裝置資訊時請重新匯入本機登入狀態。",
        "Paste a single-account JSON with cookies and the original sign-in deviceId. Any saved identity is checked against the online result; accounts are never silently switched. Re-import local sign-in state if device information is missing.",
        "Cookieと元のログイン端末のdeviceIdを含む単一アカウントのJSONを貼り付けます。既存のユーザー情報はオンライン結果と照合し、勝手に別アカウントに切り替えません。端末情報がなければローカルログイン状態を再取り込みしてください。",
        "Cookie와 원래 로그인 기기의 deviceId가 포함된 단일 계정 JSON을 붙여넣으세요. 기존 계정은 온라인 결과와 대조하며 몰래 다른 계정으로 바꾸지 않습니다. 기기 정보가 없으면 로컬 로그인 상태를 다시 가져오세요.",
        "Cole um JSON de uma conta com cookies e o deviceId original. A identidade salva é conferida online e nunca substituída silenciosamente. Importe novamente o estado local se faltarem os dados do dispositivo.",
    ]),
    ("快照文本", ["快照文字", "Snapshot text", "スナップショットのテキスト", "스냅샷 텍스트", "Texto do snapshot"]),
    ("豆包工作网页登录", ["豆包工作網頁登入", "Doubao Work browser sign-in", "Doubao Workウェブログイン", "Doubao Work 웹 로그인", "Login web do Doubao Work"]),
    ("在官方页面完成登录，桌面窗口读取本次 Cookie、设备和客户端上下文并核对账号；网页面板可粘贴完整 Cookie JSON。", [
        "在官方頁面完成登入，桌面視窗讀取本次 Cookie、裝置及用戶端資訊並核對帳號；網頁面板可貼上完整 Cookie JSON。",
        "Sign in on the official page. The desktop window captures this session's cookies, device and client information and checks the account. On web or mobile, paste the complete cookie JSON into the callback field.",
        "公式ページでログインします。デスクトップウィンドウが今回のCookie、端末情報とクライアント情報を取得し、アカウントを確認します。ウェブやモバイルでは完全なCookie JSONをコールバック欄に貼り付けます。",
        "공식 페이지에서 로그인하세요. 데스크톱 창이 이번 Cookie와 기기 및 클라이언트 정보를 읽어 계정을 확인합니다. 웹이나 모바일에서는 전체 Cookie JSON을 콜백 입력란에 붙여넣으세요.",
        "Entre na página oficial. A janela do aplicativo captura os cookies, dispositivo e contexto desta sessão e confere a conta. Na web ou no celular, cole o JSON completo de cookies no campo de retorno.",
    ]),
];

pub fn input(params: &Value) -> Result<(Value, &'static str, Option<String>)> {
    let source = &params["source"];
    match source["kind"].as_str() {
        Some("desktop") => Ok((
            agent2api_wasm_sdk::host("desktop.credentials.read", json!({}))?,
            "desktop",
            None,
        )),
        Some("file_base64") => Err(errors::invalid(
            "Cookie 数据库请通过已授权的本机导入入口读取；文本导入不接受二进制数据库",
        )),
        Some("file_text") => {
            let text = source["content"]
                .as_str()
                .ok_or_else(|| errors::invalid("豆包工作快照内容必须是文本"))?;
            Ok((credentials::payload(&json!(text))?, "file", None))
        }
        Some("manual") => {
            let payload = &source["payload"];
            if payload.get("methodIndex").is_none() {
                return Ok((credentials::payload(payload)?, "manual", None));
            }
            let index = payload["methodIndex"]
                .as_u64()
                .filter(|index| *index <= 1)
                .ok_or_else(|| errors::invalid("请选择豆包工作手工或文本快照导入"))?;
            let fields = payload["inputs"]
                .as_object()
                .ok_or_else(|| errors::invalid("豆包工作登录字段必须是对象"))?;
            let key = if index == 0 {
                "cookie"
            } else {
                "credentialText"
            };
            let allowed = if index == 0 {
                vec![key, "name", "deviceId", "webId", "clientVersion"]
            } else {
                vec![key, "name"]
            };
            if fields
                .iter()
                .any(|(name, value)| !allowed.contains(&name.as_str()) || !value.is_string())
            {
                return Err(errors::invalid("豆包工作导入包含未申报字段或非文本输入"));
            }
            let text = fields
                .get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| errors::invalid("请提供豆包工作完整 Cookie 快照"))?;
            let name = fields
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string);
            let mut input = credentials::payload(&json!(text))?;
            if index == 0 {
                if !input.is_object() {
                    input = json!({ "cookies": input });
                }
                for key in ["deviceId", "webId", "clientVersion"] {
                    if let Some(value) = fields
                        .get(key)
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                    {
                        if input
                            .get(key)
                            .and_then(Value::as_str)
                            .is_some_and(|existing| !existing.is_empty() && existing != value)
                        {
                            return Err(errors::invalid(
                                "豆包工作快照与表单中的客户端信息不一致，请保留原登录上下文",
                            ));
                        }
                        input[key] = json!(value);
                    }
                }
            }
            Ok((input, "manual", name))
        }
        _ => Err(errors::invalid("请选择豆包工作手工或文本快照导入")),
    }
}
