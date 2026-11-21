use crate::{host, json, Result, Value};

pub fn take() -> Result<Value> { host("credentials.legacy.take", json!({})) }

pub fn method(index: u64, locale: &str) -> Value {
    let (label, instruction, confirmation) = match locale {
        "en" => ("Import legacy environment credentials", "Imports the account from legacy environment credentials configured on the gateway.", "Import credentials for this provider"),
        "ja" => ("旧環境変数の認証情報を取り込む", "ゲートウェイに設定された旧環境変数の認証情報からアカウントを取り込みます。", "この提供元の認証情報を取り込む"),
        "ko" => ("기존 환경 변수 인증 가져오기", "게이트웨이에 설정된 기존 환경 변수 인증 정보에서 계정을 가져옵니다.", "이 제공업체의 인증 정보 가져오기"),
        "pt-BR" => ("Importar credenciais antigas do ambiente", "Importa a conta das credenciais antigas de variáveis de ambiente configuradas no gateway.", "Importar credenciais deste provedor"),
        "zh-Hant" => ("匯入舊環境憑據", "從目前閘道已設定的舊版環境變數中匯入帳號。", "匯入這家提供商的舊憑據"),
        _ => ("导入旧环境凭据", "从当前网关已配置的旧版环境变量中导入账号。", "导入这家提供商的旧凭据"),
    };
    json!({ "methodIndex": index, "type": "api", "label": label, "instructions": instruction,
        "prompts": [{ "key": "legacyEnvironment", "type": "select", "label": label,
            "options": [{ "value": "import", "label": confirmation }] }] })
}
