pub mod sms;
pub mod managed;

use serde::{Deserialize, Serialize};
use crate::{json, Value};

#[derive(Clone, Deserialize, Serialize)]
pub struct Field {
    pub key: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub secret: bool,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub options: Vec<OptionItem>,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct OptionItem { pub value: String, pub label: String }
#[derive(Clone, Deserialize, Serialize)]
pub struct Geetest { pub gt: String, pub challenge: String, pub success: bool, pub new_captcha: bool }
#[derive(Clone, Deserialize, Serialize)]
pub struct Interaction {
    pub id: String,
    pub title: String,
    pub message: String,
    pub fields: Vec<Field>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captcha: Option<Geetest>,
}

pub fn desktop_method(index: u64, client: &str, locale: &str) -> Value {
    let (label, instructions) = match locale {
        "en" => ("Import local sign-in", format!("Reads only the official {client} sign-in files on the computer running the connected gateway, not this phone or other clients. Files are never changed; no extra helper installation or permission setup is needed. Sign in to the official client first.")),
        "ja" => ("ローカルログインを取り込む", format!("接続先ゲートウェイのコンピューターにある公式 {client} のログインファイルだけを読み取ります。このスマートフォンや他のクライアントは読み取りません。ファイルの変更、補助プラグインのインストールや追加の権限設定は不要です。先に公式クライアントにログインしてください。")),
        "ko" => ("로컬 로그인 가져오기", format!("연결된 게이트웨이가 실행되는 컴퓨터의 공식 {client} 로그인 파일만 읽습니다. 이 휴대전화나 다른 클라이언트는 읽지 않습니다. 파일을 수정하지 않으며 추가 도우미 설치나 권한 설정이 필요하지 않습니다. 먼저 공식 클라이언트에 로그인하세요.")),
        "pt-BR" => ("Importar login local", format!("Lê apenas os arquivos de login do cliente oficial {client} no computador do gateway conectado, não neste celular ou em outros clientes. Não altera arquivos e não exige instalar auxiliares nem configurar permissões extras. Entre primeiro no cliente oficial.")),
        "zh-Hant" => ("匯入本機登入態", format!("只讀取所連接閘道電腦上的 {client} 官方登入檔案，不讀取這部手機或其他用戶端、不修改檔案，也不需要額外安裝或授權輔助外掛。請先在該官方用戶端登入。")),
        _ => ("导入本机登录态", format!("只读取所连接网关电脑上的 {client} 官方登录文件，不读取这部手机或其他客户端、不修改文件，也不需要额外安装或授权辅助插件。请先在该官方客户端登录。")),
    };
    json!({ "methodIndex": index, "type": "desktop", "label": label, "instructions": instructions,
        "prompts": [], "localOnly": true, "browserMode": "none", "manualCallback": false, "callbackInput": "url" })
}

pub fn start(flow: &str, interaction: Interaction) -> Value {
    json!({ "flow": flow, "url": "", "poll_interval_ms": 1500, "instructions": interaction.message,
        "interaction": interaction })
}
pub fn pending(interaction: Option<Interaction>) -> Value { json!({ "state": "pending", "interaction": interaction }) }
pub fn done(account: Value) -> Value { json!({ "state": "done", "account": account }) }
