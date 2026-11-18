use crate::{host, json, Result, Value};

pub fn take() -> Result<Value> { host("credentials.legacy.take", json!({})) }

pub fn method(index: u64, locale: &str) -> Value {
    let (label, instruction, confirmation) = match locale {
        "en" => ("Import legacy environment credentials", "Reads only this provider's legacy credentials from the connected gateway process. Saves an ordinary account; it does not enable anonymous access or read arbitrary environment variables.", "Import credentials for this provider"),
        "ja" => ("旧環境変数の認証情報を取り込む", "接続先ゲートウェイのプロセスから、この提供元の旧認証情報だけを読み取ります。通常アカウントとして保存し、匿名アクセスや任意の環境変数の読み取りは有効にしません。", "この提供元の認証情報を取り込む"),
        "ko" => ("기존 환경 변수 인증 가져오기", "연결된 게이트웨이 프로세스에서 이 제공업체의 기존 인증 정보만 읽습니다. 일반 계정으로 저장하며 익명 접근이나 임의 환경 변수 읽기를 허용하지 않습니다.", "이 제공업체의 인증 정보 가져오기"),
        "pt-BR" => ("Importar credenciais antigas do ambiente", "Lê apenas as credenciais antigas deste provedor no processo do gateway conectado. Salva uma conta normal, sem ativar acesso anônimo nem ler variáveis arbitrárias.", "Importar credenciais deste provedor"),
        "zh-Hant" => ("匯入舊環境憑據", "只讀所連接閘道程序中這家提供商的舊憑據，保存為普通帳號；不開啟零帳號存取，也不讀取其他環境變數。", "匯入這家提供商的舊憑據"),
        _ => ("导入旧环境凭据", "只读所连接网关进程中这家提供商的旧凭据，保存为普通账号；不开启零账号访问，也不读取其他环境变量。", "导入这家提供商的旧凭据"),
    };
    json!({ "methodIndex": index, "type": "api", "label": label, "instructions": instruction,
        "prompts": [{ "key": "legacyEnvironment", "type": "select", "label": label,
            "options": [{ "value": "import", "label": confirmation }] }] })
}
