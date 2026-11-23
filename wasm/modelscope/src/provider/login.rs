//! 声明式令牌/文本导入表单；无浏览器登录、无本机文件访问。

use agent2api_wasm_sdk::{json, Value};

use super::Region;

pub fn methods(region: Region, locale: &str) -> Value {
    let (token, placeholder, instructions, import, credential, import_hint) = match locale {
        "en" => (
            "Enter token", "Paste a token starting with ms-",
            "Paste a token from the corresponding ModelScope site to add the account.",
            "Import credentials text", "Credentials JSON or token",
            "Select or paste a ModelScope credential file or token to import the account.",
        ),
        "zh-Hant" => (
            "貼上權杖", "貼上 ms- 開頭的權杖",
            "貼上對應魔搭站點的權杖即可新增帳號。",
            "匯入憑證文字", "憑證 JSON 或權杖",
            "選擇或貼上魔搭憑據檔案或權杖即可匯入帳號。",
        ),
        "ja" => (
            "トークンを入力", "ms- で始まるトークンを貼り付け",
            "対応する ModelScope サイトのトークンを貼り付けてアカウントを追加できます。",
            "認証テキストを取り込む", "認証 JSON またはトークン",
            "ModelScope の認証ファイルまたはトークンを選択・貼り付けてアカウントを取り込めます。",
        ),
        "ko" => (
            "토큰 입력", "ms-로 시작하는 토큰 붙여넣기",
            "해당 ModelScope 사이트의 토큰을 붙여넣어 계정을 추가할 수 있습니다.",
            "인증 텍스트 가져오기", "인증 JSON 또는 토큰",
            "ModelScope 인증 파일 또는 토큰을 선택하거나 붙여넣어 계정을 가져올 수 있습니다.",
        ),
        "pt-BR" => (
            "Inserir token", "Cole um token que comece com ms-",
            "Cole um token do site ModelScope correspondente para adicionar a conta.",
            "Importar texto de credenciais", "JSON de credenciais ou token",
            "Selecione ou cole um arquivo de credenciais ou token do ModelScope para importar a conta.",
        ),
        _ => (
            "粘贴令牌", "粘贴 ms- 开头的令牌",
            "粘贴对应魔搭站点的令牌即可添加账号。",
            "导入凭证文本", "凭证 JSON 或令牌",
            "选择或粘贴魔搭凭据文件或令牌即可导入账号。",
        ),
    };
    let _ = region;
    json!({ "methods": [
        { "methodIndex": 0, "type": "api", "label": token, "prompts": [
            { "key": "accessToken", "type": "text", "label": "ms- Token", "secret": true, "placeholder": placeholder }
          ], "instructions": instructions },
        { "methodIndex": 1, "type": "api", "label": import, "prompts": [
            { "key": "credentialText", "type": "text", "label": credential, "secret": true }
          ], "instructions": import_hint }
    ] })
}
