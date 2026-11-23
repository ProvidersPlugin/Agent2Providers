//! 只替换共享实现自己产生的地域前缀，不改账号、工具内容或上游原文。
//! 分类、脱敏与标准错误帧继续复用同一套规则。

use crate::shared_errors;
use agent2api_wasm_sdk::{json, Error, Value};

pub use shared_errors::{code, host_stop, message, redact, sanitize, transport};

fn own_message(message: &str) -> String {
    if let Some(rest) = message.strip_prefix("Accio 国际版请求失败") {
        return format!("Accio 国内版请求失败{rest}");
    }
    match message {
        "Accio 国际版上游请求失败" => "Accio 国内版上游请求失败".into(),
        "请从 Accio 国际版目录选择一个模型" => {
            "请从 Accio 国内版目录选择一个模型".into()
        }
        "当前 Accio 国际版模型未声明图片输入能力" => {
            "当前 Accio 国内版模型未声明图片输入能力".into()
        }
        "Accio 回调与本次国际版授权不匹配" => {
            "Accio 回调与本次国内版授权不匹配".into()
        }
        other => other.to_string(),
    }
}

pub fn domestic(mut error: Error) -> Error {
    error.message = own_message(&error.message);
    error
}

pub fn upstream(status: u16, code: &str, message: &str, secrets: &[&str]) -> Error {
    // 当前国内 ADK 的 402 可能带 unauthorized 文案，但它是积分闸门，不消耗刷新链。
    let effective = if status == 402 || (200..300).contains(&status) && code == "402" {
        429
    } else {
        status
    };
    domestic(shared_errors::upstream(effective, code, message, secrets))
}

pub fn classify(status: u16, body: &Value) -> Value {
    let business = [
        "upstreamCode",
        "upstream_code",
        "code",
        "errorCode",
        "error_code",
    ]
    .iter()
    .find_map(|key| shared_errors::number(&body[*key]));
    let effective = if status == 402 || (200..300).contains(&status) && business == Some(402) {
        429
    } else {
        status
    };
    let mut value = shared_errors::classify(effective, body);
    if business == Some(402) && (200..300).contains(&status) {
        value["upstream_code"] = json!(402);
    }
    value["message"] = json!(own_message(value["message"].as_str().unwrap_or("")));
    value
}

pub fn event(error: &Error) -> Value {
    shared_errors::event(&domestic(error.clone()))
}

pub fn login_text(mut value: Value) -> Value {
    for key in ["hint", "message"] {
        if let Some(text) = value[key].as_str() {
            let text = if text == "等待 Accio 国际版授权回调" {
                "等待 Accio 国内版授权回调".into()
            } else {
                own_message(text)
            };
            value[key] = json!(text);
        }
    }
    value
}
