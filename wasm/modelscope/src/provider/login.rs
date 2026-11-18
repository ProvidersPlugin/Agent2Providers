//! 声明式令牌/文本导入表单；无浏览器登录、无本机文件访问。

use agent2api_wasm_sdk::{json, Value};

use super::Region;

pub fn methods(region: Region, locale: &str) -> Value {
    let (token, placeholder, instructions, import, credential, import_hint) = match locale {
        "en" => (
            "Enter token", "Paste a token starting with ms-",
            "Use a token from the selected ModelScope site. Accounts are separate; importing verifies your identity and may claim today's magicubes.",
            "Import credentials text", "Credentials JSON or token",
            "Paste a token (with or without Bearer) or JSON with accessToken/access_token, apiKey/api_key, token or session, including a secret/auth wrapper. Binary files and desktop credentials are not implemented.",
        ),
        "zh-Hant" => (
            "貼上權杖", "貼上 ms- 開頭的權杖",
            "請使用所選魔搭站點的權杖，兩站帳號互不通用。匯入時會校驗身分，也可能觸發當日魔粒發放。",
            "匯入憑證文字", "憑證 JSON 或權杖",
            "貼上權杖（可含 Bearer 前綴）或含 accessToken/access_token、apiKey/api_key、token、session 的 JSON，支援 secret/auth 容器；二進位檔案與本機登入態尚未接入。",
        ),
        "ja" => (
            "トークンを入力", "ms- で始まるトークンを貼り付け",
            "選択した ModelScope サイトのトークンを使用してください。アカウントは地域別です。取り込み時に本人情報を確認し、当日の魔粒が付与される場合があります。",
            "認証テキストを取り込む", "認証 JSON またはトークン",
            "トークン（Bearer 接頭辞も可）、または accessToken/access_token、apiKey/api_key、token、session を含む JSON を貼り付けてください。secret/auth の入れ子も読み取ります。バイナリファイルとローカル認証情報の取り込みは未実装です。",
        ),
        "ko" => (
            "토큰 입력", "ms-로 시작하는 토큰 붙여넣기",
            "선택한 ModelScope 사이트의 토큰을 사용하세요. 지역별 계정은 별개입니다. 가져올 때 계정을 확인하며 오늘의 마법 큐브가 지급될 수 있습니다.",
            "인증 텍스트 가져오기", "인증 JSON 또는 토큰",
            "토큰(Bearer 접두사 포함 가능) 또는 accessToken/access_token, apiKey/api_key, token, session이 포함된 JSON을 붙여넣으세요. secret/auth 중첩도 읽습니다. 바이너리 파일과 로컬 인증 정보 가져오기는 아직 구현되지 않았습니다.",
        ),
        "pt-BR" => (
            "Inserir token", "Cole um token que comece com ms-",
            "Use um token do site ModelScope selecionado. As contas são separadas; a importação verifica sua identidade e pode receber os magicubes do dia.",
            "Importar texto de credenciais", "JSON de credenciais ou token",
            "Cole um token (com ou sem Bearer) ou JSON com accessToken/access_token, apiKey/api_key, token ou session; contêineres secret/auth também são aceitos. A importação binária e de credenciais locais ainda não foi implementada.",
        ),
        _ => (
            "粘贴令牌", "粘贴 ms- 开头的令牌",
            "请使用所选魔搭站点的令牌，两站账号互不通用。导入时会校验身份，也可能触发当日魔粒发放。",
            "导入凭证文本", "凭证 JSON 或令牌",
            "粘贴令牌（可带 Bearer 前缀）或包含 accessToken/access_token、apiKey/api_key、token、session 的 JSON，支持 secret/auth 容器；二进制文件与本机登录态尚未接入。",
        ),
    };
    json!({ "methods": [
        { "methodIndex": 0, "type": "api", "label": token, "prompts": [
            { "key": "accessToken", "type": "text", "label": "ms- Token", "secret": true, "placeholder": placeholder }
          ], "instructions": format!("{instructions}\n{}", region.site_base()) },
        { "methodIndex": 1, "type": "api", "label": import, "prompts": [
            { "key": "credentialText", "type": "text", "label": credential, "secret": true }
          ], "instructions": import_hint }
    ] })
}
