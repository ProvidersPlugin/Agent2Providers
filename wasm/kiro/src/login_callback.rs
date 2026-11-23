//! 两阶段回调只来自 managed.take_input 一次交付的 {url}；不从自由参数取code或state。
//! 门户 localhost 两固定路径；AWS 仅 127.0.0.1/oauth/callback；host oauthState 独立于flow。

use crate::{
    endpoints,
    login_stage::{self, Stage},
    util,
};
use agent2api_wasm_sdk::{Error, Result, Value};
use std::collections::HashMap;

pub struct Callback {
    pub url: url::Url,
    pub query: HashMap<String, String>,
    pub kind: Kind,
}
#[derive(Clone, Copy)]
pub enum Kind {
    Social,
    AwsChoice,
    AwsCode,
    Denied,
}
impl Callback {
    pub fn get(&self, key: &str) -> &str {
        self.query.get(key).map(String::as_str).unwrap_or("")
    }
}
fn bad() -> Error {
    Error::http(400, "Kiro回调不属于本次授权阶段或输入冲突，未发送授权码")
        .with_code("kiro_callback_invalid")
}

pub fn parse(input: &Value, stage: &Stage, phase: &str) -> Result<Callback> {
    login_stage::exact(input, &["url"])?;
    let raw = input["url"]
        .as_str()
        .filter(|raw| {
            !raw.is_empty()
                && raw.len() <= 8192
                && !raw.chars().any(|ch| ch.is_whitespace() || ch.is_control())
                && !raw
                    .bytes()
                    .any(|byte| byte <= b' ' || byte == 127 || byte == b'\\')
        })
        .ok_or_else(bad)?;
    let bytes = raw.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return Err(bad());
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    let url = url::Url::parse(raw).map_err(|_| bad())?;
    let host = match phase {
        "portal" => "localhost",
        "aws" => "127.0.0.1",
        _ => return Err(bad()),
    };
    if url.scheme() != "http"
        || url.host_str() != Some(host)
        || url.port() != Some(stage.port)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !stage
            .callback_urls
            .iter()
            .any(|allowed| allowed == raw.split('?').next().unwrap_or(""))
        || phase == "aws" && url.path() != "/oauth/callback"
    {
        return Err(bad());
    }
    let expected = format!("http://{host}:{}{}", stage.port, url.path());
    if raw.split('?').next() != Some(expected.as_str()) {
        return Err(bad());
    }
    let mut query = HashMap::new();
    for (key, value) in url.query_pairs() {
        let allowed = match phase {
            "portal" => matches!(
                key.as_ref(),
                "state"
                    | "code"
                    | "login_option"
                    | "issuer_url"
                    | "idc_region"
                    | "error"
                    | "error_description"
                    | "error_uri"
            ),
            "aws" => matches!(
                key.as_ref(),
                "state" | "code" | "error" | "error_description" | "error_uri"
            ),
            _ => false,
        };
        if !allowed
            || value.len() > 8192
            || value.chars().any(char::is_control)
            || key.contains('\u{fffd}')
            || value.contains('\u{fffd}')
            || query.insert(key.into_owned(), value.into_owned()).is_some()
        {
            return Err(bad());
        }
    }
    if query.get("state").map(String::as_str) != Some(stage.oauth_state.as_str()) {
        return Err(bad());
    }
    let code = query.get("code");
    let error = query.get("error");
    let option = query.get("login_option").map(String::as_str).unwrap_or("");
    let kind = if let Some(error) = error {
        if error.is_empty()
            || error.len() > 256
            || error.chars().any(char::is_whitespace)
            || code.is_some()
            || ["issuer_url", "idc_region"]
                .iter()
                .any(|key| query.contains_key(*key))
            || query.contains_key("login_option")
                && (phase != "portal"
                    || !matches!(
                        option,
                        "google" | "github" | "builderid" | "awsidc" | "internal"
                    ))
        {
            return Err(bad());
        }
        Kind::Denied
    } else if phase == "aws" {
        if query.contains_key("error_description")
            || query.contains_key("error_uri")
            || code.is_none_or(|code| !valid_code(code))
        {
            return Err(bad());
        }
        Kind::AwsCode
    } else if matches!(option, "google" | "github") {
        if code.is_none_or(|code| !valid_code(code))
            || ["issuer_url", "idc_region", "error_description", "error_uri"]
                .iter()
                .any(|key| query.contains_key(*key))
        {
            return Err(bad());
        }
        Kind::Social
    } else if matches!(option, "builderid" | "awsidc" | "internal") {
        if code.is_some()
            || query.contains_key("error_description")
            || query.contains_key("error_uri")
        {
            return Err(bad());
        }
        let issuer = query
            .get("issuer_url")
            .map(String::as_str)
            .ok_or_else(bad)?;
        let region = query
            .get("idc_region")
            .map(String::as_str)
            .ok_or_else(bad)?;
        if !endpoints::https(issuer) {
            return Err(bad());
        }
        let issuer_url = url::Url::parse(issuer).map_err(|_| bad())?;
        if issuer.len() > 4096
            || issuer_url.port_or_known_default() != Some(443)
            || issuer_url.query().is_some()
        {
            return Err(bad());
        }
        endpoints::auth_region(region)?;
        Kind::AwsChoice
    } else {
        // 外部IdP/未知分支尚未迁移，不能拿它的参数选择另一种换码路径。
        return Err(bad());
    };
    Ok(Callback { url, query, kind })
}
fn valid_code(code: &str) -> bool {
    util::identifier(code, 4096) && !code.chars().any(char::is_whitespace)
}
