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
        "在 www.accio-ai.com 完成登录即可添加账号；远程访问面板时，按提示粘贴登录后的回调地址即可。",
        "Sign in at www.accio-ai.com to add the account; when accessing the panel remotely, paste the callback URL after signing in.",
        "在 www.accio-ai.com 完成登入即可新增帳號；遠端存取面板時，按提示貼上登入後的回呼網址即可。",
        "www.accio-ai.com でログインするとアカウントを追加できます。リモート画面ではログイン後にコールバック URL を貼り付けてください。",
        "www.accio-ai.com에서 로그인하면 계정이 추가됩니다. 원격 패널에서는 로그인 후 콜백 URL을 붙여넣으세요.",
        "Conclua o login em www.accio-ai.com para adicionar a conta; no acesso remoto ao painel, cole a URL de callback após o login.",
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
                "填写国内版访问令牌；填写刷新令牌后支持自动续期。",
                "Enter a mainland access token; include a refresh token to enable automatic renewal.",
                "填寫國內版存取權杖；填寫更新權杖後支援自動續期。",
                "中国国内版のアクセストークンを入力してください。更新トークンを入力すると自動更新に対応します。",
                "중국 국내판 액세스 토큰을 입력하세요. 갱신 토큰을 함께 입력하면 자동 갱신을 지원합니다.",
                "Insira o token de acesso da China; informe o token de renovação para ativar a renovação automática.",
            ])),
            Some(2) => method["instructions"] = json!(localized(locale, [
                "选择或粘贴 Accio 国内版凭据文件即可导入账号；包含完整登录信息时支持自动续期。",
                "Select or paste an Accio mainland credential file to import the account; automatic renewal is supported when complete sign-in details are included.",
                "選擇或貼上 Accio 國內版憑據檔案即可匯入帳號；包含完整登入資訊時支援自動續期。",
                "Accio 中国国内版の認証ファイルを選択または貼り付けてアカウントを取り込めます。完全なログイン情報が含まれていれば自動更新に対応します。",
                "Accio 중국 국내판 인증 파일을 선택하거나 붙여넣어 계정을 가져올 수 있으며, 전체 로그인 정보가 포함되어 있으면 자동 갱신을 지원합니다.",
                "Selecione ou cole um arquivo de credenciais do Accio da China para importar a conta; a renovação automática é suportada quando os dados completos de login estão incluídos.",
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
