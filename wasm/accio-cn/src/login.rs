//! 共用 PKCE 与回调状态机绑定本包国内端点，浏览器模式不是凭据地区。
//! 登录文案由当前界面语言决定，不重写账号资料或上游错误内容。

use crate::{credentials, errors, shared_login};
use agent2api_wasm_sdk::{json, Result, Value};

fn localized(locale: &str, values: [&str; 6]) -> String {
    values[match locale {
        "en" => 1,
        "zh-Hant" => 2,
        "ja" => 3,
        "ko" => 4,
        "pt-BR" => 5,
        _ => 0,
    }]
    .into()
}

fn instructions(locale: &str) -> String {
    localized(locale, [
        "在 www.accio-ai.com 完成国内账号授权；同机自动接收，远程部署可粘贴本次完整回调地址。",
        "Authorize your mainland account at www.accio-ai.com. The gateway computer receives the callback automatically; remote users can paste this login's complete callback URL.",
        "在 www.accio-ai.com 完成國內帳號授權；同機自動接收，遠端部署可貼上本次完整回呼位址。",
        "www.accio-ai.com で中国国内版のアカウントを認証します。ゲートウェイでは自動受信し、リモートでは今回の完全なコールバック URL を貼り付けられます。",
        "www.accio-ai.com에서 중국 국내판 계정을 인증하세요. 게이트웨이 컴퓨터에서는 자동 수신하며, 원격에서는 이번 로그인의 전체 콜백 URL을 붙여넣을 수 있습니다.",
        "Autorize a conta da China em www.accio-ai.com. O computador do gateway recebe o retorno automaticamente; remotamente, cole a URL completa de retorno deste login.",
    ])
}

pub fn methods(locale: &str) -> Value {
    let mut value = shared_login::methods_for(locale);
    for method in value["methods"].as_array_mut().into_iter().flatten() {
        match method["methodIndex"].as_u64() {
            Some(0) => {
                method["label"] = json!(localized(locale, [
                    "Accio 国内版网页登录", "Accio mainland web sign-in", "Accio 國內版網頁登入",
                    "Accio 中国国内版のウェブログイン", "Accio 중국 국내판 웹 로그인", "Login web do Accio na China",
                ]));
                method["instructions"] = json!(instructions(locale));
            }
            Some(1) => method["instructions"] = json!(localized(locale, [
                "请填写国内版访问令牌；国际账号不能共用。刷新令牌可选，未提供时无法自动续期。",
                "Enter a mainland access token; international accounts are separate. The optional refresh token is required for automatic renewal.",
                "請填寫國內版存取權杖；國際帳號不能共用。更新權杖可選，未提供時無法自動續期。",
                "中国国内版のアクセストークンを入力してください。国際版のアカウントとは別です。自動更新には任意の更新トークンが必要です。",
                "중국 국내판 액세스 토큰을 입력하세요. 국제판 계정과 별개이며, 자동 갱신에는 선택 사항인 갱신 토큰이 필요합니다.",
                "Insira o token de acesso da China; as contas internacionais são separadas. O token de renovação é opcional e necessário para renovação automática.",
            ])),
            Some(2) => method["instructions"] = json!(localized(locale, [
                "接受国内版 accessToken、refreshToken、expiresAt、userId、deviceId、utdid、sgcookie 和 secret/pluginSecret 包装；不会读取本机文件。",
                "Accepts mainland accessToken, refreshToken, expiresAt, userId, deviceId, utdid and sgcookie, including secret/pluginSecret wrappers. No local files are read.",
                "接受國內版 accessToken、refreshToken、expiresAt、userId、deviceId、utdid、sgcookie 和 secret/pluginSecret 包裝；不讀取本機檔案。",
                "中国国内版の accessToken、refreshToken、expiresAt、userId、deviceId、utdid、sgcookie と secret/pluginSecret 形式を受け付けます。ローカルファイルは読みません。",
                "중국 국내판 accessToken, refreshToken, expiresAt, userId, deviceId, utdid, sgcookie 및 secret/pluginSecret 형식을 지원합니다. 로컬 파일은 읽지 않습니다.",
                "Aceita accessToken, refreshToken, expiresAt, userId, deviceId, utdid e sgcookie da China, inclusive em secret/pluginSecret. Não lê arquivos locais.",
            ])),
            _ => {}
        }
    }
    value
}

pub fn start(params: &Value) -> Result<Value> {
    let mut hints = params["payload"].clone();
    // 宿主 mode 是 external/embedded/auto 的浏览器选择，不能当成 cn/intl 凭据地区。
    if hints["mode"]
        .as_str()
        .is_some_and(|mode| matches!(mode, "external" | "embedded" | "auto"))
    {
        if let Some(fields) = hints.as_object_mut() {
            fields.remove("mode");
        }
    }
    credentials::check_region(&hints)?;
    let mut value = shared_login::start(params)?;
    value["instructions"] = json!(instructions(
        params["payload"]["locale"].as_str().unwrap_or("zh-CN")
    ));
    Ok(value)
}

pub fn callback(params: &Value) -> Result<Value> {
    shared_login::callback(params).map(errors::login_text)
}
pub fn poll(params: &Value) -> Result<Value> {
    shared_login::poll(params).map(errors::login_text)
}
