pub mod sms;
pub mod managed;
mod methods;
pub use methods::prepare_methods;

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
        "en" => ("Import local sign-in", format!("Imports the signed-in {client} account from the gateway computer. Sign in to the official client first.")),
        "ja" => ("ローカルログインを取り込む", format!("ゲートウェイのコンピューターでログイン済みの {client} クライアントからアカウントを取り込みます。先にクライアントでログインしてください。")),
        "ko" => ("로컬 로그인 가져오기", format!("게이트웨이 컴퓨터에 로그인된 {client} 클라이언트에서 계정을 가져옵니다. 먼저 클라이언트에 로그인하세요.")),
        "pt-BR" => ("Importar login local", format!("Importa a conta conectada no cliente {client} do computador do gateway. Entre primeiro no cliente oficial.")),
        "zh-Hant" => ("匯入本機登入態", format!("讀取閘道所在電腦上已登入的 {client} 用戶端帳號，請先在用戶端完成登入。")),
        _ => ("导入本机登录态", format!("读取网关所在电脑上已登录的 {client} 客户端账号，请先在客户端完成登录。")),
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
