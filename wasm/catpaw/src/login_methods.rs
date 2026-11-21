//! 声明式入口由宿主绘制；网页登录只允许宿主托管，手工快照和本机实时来源仍分开。

use agent2api_wasm_sdk::{json, Result, Value};

use crate::{errors, gates};

pub fn web_text(locale: &str) -> [&'static str; 2] {
    match locale {
        "en" => ["Sign in to CatPaw", "Complete CatPaw sign-in in the external browser. The host automatically receives the callback or checks this login session, then verifies the account. Do not paste tokens or callback URLs."],
        "ja" => ["CatPaw にログイン", "外部ブラウザーで CatPaw にログインしてください。ホストが今回のコールバックまたはログイン状態を受け取り、アカウントを確認します。トークンやコールバック URL の貼り付けは不要です。"],
        "ko" => ["CatPaw 로그인", "외부 브라우저에서 CatPaw 로그인을 완료하세요. 호스트가 이번 콜백 또는 로그인 상태를 자동으로 받아 계정을 확인합니다. 토큰이나 콜백 URL을 붙여 넣지 마세요."],
        "pt-BR" => ["Entrar no CatPaw", "Conclua o login no CatPaw no navegador externo. O host recebe o retorno ou consulta esta sessão e valida a conta. Não cole tokens nem URLs de retorno."],
        "zh-Hant" => ["CatPaw 網頁登入", "請在外部瀏覽器完成 CatPaw 登入。主程式會自動接收本次回呼或輪詢登入結果，再核實帳號；不要貼上權杖或回呼網址。"],
        _ => ["CatPaw 网页登录", "请在外部浏览器完成 CatPaw 登录。主程序会自动接收本次回调或轮询登录结果，再核实账号；不要粘贴令牌或回调地址。"],
    }
}

pub fn methods(locale: &str) -> Value {
    let web = web_text(locale);
    let manual = manual_text(locale);
    let snapshot = snapshot_text(locale);
    let mut desktop = agent2api_wasm_sdk::login::desktop_method(3, "CatPaw", locale);
    desktop["instructions"] = json!(desktop_text(locale));
    json!({ "instructions": gates::summary(locale), "methods": [
        { "methodIndex": 0, "type": "oauth", "label": web[0], "instructions": web[1], "prompts": [],
            "browserMode": "choice", "manualCallback": false, "callbackInput": "url", "validatesCallback": true },
        { "methodIndex": 1, "type": "api", "label": manual[0],
            "instructions": manual[1],
            "prompts": [
                { "key": "token", "type": "text", "label": manual[2], "secret": true },
                { "key": "uid", "type": "text", "label": manual[3], "optional": true },
                { "key": "loginName", "type": "text", "label": manual[4], "optional": true },
                { "key": "name", "type": "text", "label": manual[5], "optional": true }
            ] },
        { "methodIndex": 2, "type": "api", "label": snapshot[0],
            "instructions": snapshot[1],
            "prompts": [
                { "key": "credentialJson", "type": "text", "label": snapshot[2], "secret": true },
                { "key": "name", "type": "text", "label": manual[5], "optional": true }
            ] },
        desktop,
        agent2api_wasm_sdk::legacy_credentials::method(4, locale)
    ] })
}

fn manual_text(locale: &str) -> [&'static str; 6] {
    match locale {
        "en" => ["Manual credential snapshot", "Provide the token and at least one user identifier (UID or sign-in name), or paste the complete auth.json into the token field. Import checks format; actual use checks the account and its current model catalog.", "Sign-in token / credential JSON", "UID (or sign-in name required)", "Sign-in name", "Account note"],
        "ja" => ["認証情報を手動入力", "トークンと UID またはログイン名を入力してください。トークン欄に auth.json 全体を貼り付けることもできます。追加時は形式を検査し、使用時にアカウントと現在のモデル一覧を確認します。", "ログイントークン / 認証情報 JSON", "UID（ログイン名といずれか必須）", "ログイン名", "メモ名"],
        "ko" => ["인증 정보 직접 입력", "토큰과 UID 또는 로그인 이름을 입력하거나, 토큰 입력란에 auth.json 전체를 붙여 넣으세요. 추가 시 형식을 검사하며 실제 사용 시 계정과 현재 모델 목록을 확인합니다.", "로그인 토큰 / 인증 정보 JSON", "UID (로그인 이름과 둘 중 하나 필수)", "로그인 이름", "계정 메모"],
        "pt-BR" => ["Credenciais manuais", "Informe o token e ao menos um identificador (UID ou nome de login), ou cole todo o auth.json no campo de token. A importação verifica o formato; o uso verifica a conta e seu catálogo atual de modelos.", "Token de login / JSON de credenciais", "UID (ou nome de login obrigatório)", "Nome de login", "Nota da conta"],
        "zh-Hant" => ["手動憑據快照", "請提供權杖與 uid／登入名稱至少一項使用者標識；也可把完整 auth.json 貼入權杖欄。新增時校驗格式，實際使用時由主程式核對身分和目前模型目錄。", "登入權杖 / 憑據 JSON", "uid（與登入名稱至少填一項）", "登入名稱", "備註名"],
        _ => ["手工凭据快照", "请提供令牌与 uid/登录名至少一项用户标识；也可把完整 auth.json 粘入令牌框。添加时校验格式，实际使用时由主程序核对身份和当前模型目录。", "登录令牌 / 凭据 JSON", "uid（与登录名至少填一项）", "登录名", "备注名"],
    }
}

fn snapshot_text(locale: &str) -> [&'static str; 3] {
    match locale {
        "en" => ["JSON credential snapshot", "Accepts one account, a legacy account record, or official auth.json content. Saves only the explicitly supplied snapshot; does not read file paths or convert a desktop-auth live reference into a snapshot.", "Credential JSON"],
        "ja" => ["JSON 認証情報のスナップショット", "単一アカウント、旧アカウント記録、公式 auth.json の内容を受け付けます。明示的に提供したスナップショットのみ保存し、ファイルパスを読み取ったり desktop-auth のリアルタイム参照を変換したりしません。", "認証情報 JSON"],
        "ko" => ["JSON 인증 정보 스냅샷", "단일 계정, 이전 계정 기록 또는 공식 auth.json 내용을 받습니다. 명시적으로 제공한 스냅샷만 저장하며 파일 경로를 읽거나 desktop-auth 실시간 참조를 스냅샷으로 바꾸지 않습니다.", "인증 정보 JSON"],
        "pt-BR" => ["Snapshot JSON de credenciais", "Aceita uma conta, um registro antigo ou o conteúdo oficial de auth.json. Salva apenas o snapshot fornecido; não lê caminhos de arquivos nem converte a referência desktop-auth em snapshot.", "JSON de credenciais"],
        "zh-Hant" => ["文字 JSON 快照", "接受單帳號、舊帳號記錄或官方 auth.json 內容；只保留本次明確提供的快照，不讀取檔案路徑，不把 desktop-auth 即時來源轉成快照。", "憑據 JSON"],
        _ => ["文本 JSON 快照", "接受单账号、旧账号记录或官方 auth.json 内容；只保留本次明确提供的快照，不读取文件路径，不把 desktop-auth 实时来源转成快照。", "凭据 JSON"],
    }
}

fn desktop_text(locale: &str) -> &'static str {
    match locale {
        "en" => "Links to CatPaw sign-in on the computer running the connected gateway, not this phone. Each use reads the current official client sign-in; switching users in CatPaw also switches this reference. No token snapshot is saved, no files are changed, and no extra permission setup is needed.",
        "ja" => "接続先ゲートウェイのコンピューター上にある CatPaw のログインを参照します。このスマートフォンではありません。使用のたびに公式クライアントの現在のログインを読み取り、CatPaw でユーザーを切り替えるとこの参照も切り替わります。トークンの保存、ファイルの変更、追加の権限設定は行いません。",
        "ko" => "이 휴대전화가 아닌 연결된 게이트웨이 컴퓨터의 CatPaw 로그인에 연결합니다. 사용할 때마다 공식 클라이언트의 현재 로그인을 읽으며 CatPaw에서 사용자를 바꾸면 이 참조도 바뀝니다. 토큰 스냅샷을 저장하거나 파일을 수정하지 않고 추가 권한 설정도 필요하지 않습니다.",
        "pt-BR" => "Vincula ao login do CatPaw no computador do gateway conectado, não neste celular. Cada uso lê o login atual do cliente oficial; trocar de usuário no CatPaw também muda esta referência. Não salva tokens, não altera arquivos e não exige permissões extras.",
        "zh-Hant" => "關聯所連接閘道電腦上的 CatPaw 登入態，不是這部手機。每次使用只讀目前官方用戶端登入；在 CatPaw 切換使用者後，此關聯也會隨之切換。不保存權杖快照、不修改檔案，也無需另外配置權限。",
        _ => "关联所连接网关电脑上的 CatPaw 登录态，不是这部手机。每次使用只读当前官方客户端登录；在 CatPaw 切换用户后，此关联也会随之切换。不保存令牌快照、不修改文件，也无需另外配置权限。",
    }
}

pub fn start(params: &Value) -> Result<Value> {
    let index = params["payload"]["methodIndex"].as_u64();
    if index == Some(3) || params["method"] == "desktop" { return crate::desktop::start(params); }
    if matches!(index, Some(1 | 2)) {
        return Err(errors::invalid("CatPaw 手工凭据或文本 JSON 应从添加账号入口提交"));
    }
    crate::web_login::start(params)
}
