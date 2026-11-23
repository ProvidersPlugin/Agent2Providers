//! 优先getJsToken，null/未知格式才回落同源活动页文本。沿用本地最新原生修复。
//! 页面不执行、跳转不跟随、完整响应1MiB上限；签名token不写台账/公开视图/错误。

use super::token_text;
use crate::{credentials::Credentials, endpoints, errors, network, util};
use agent2api_wasm_sdk::{Error, Result};

pub fn fetch(c: &Credentials) -> Result<String> {
    let result = network::get(
        &format!("{}{}", endpoints::BASE, endpoints::ACTIVITY_TOKEN),
        network::activity_headers(c),
        &c.secrets(),
    );
    match result {
        Ok(value) => {
            let code = util::integer(&value["errno"]);
            if matches!(code, Some(-6 | 1000004)) {
                return Err(errors::upstream(401, code, &value, &c.secrets()));
            }
            if code.is_none() || code == Some(0) {
                let raw = value
                    .pointer("/data/jsToken")
                    .or_else(|| value.get("jsToken"))
                    .and_then(|value| value.as_str());
                if let Some(token) = raw.and_then(token_text::extract) {
                    return Ok(token);
                }
            }
        }
        Err(error) if errors::host_stop(&error) || error.status == Some(401) => return Err(error),
        Err(_) => {}
    }
    let html = network::activity_page(c)?;
    token_text::extract(&html).ok_or_else(|| {
        Error::http(
            502,
            "库库签到凭证接口与活动页均未提供可识别jsToken，未执行任何脚本",
        )
        .with_code("kuku_activity_token_missing")
    })
}
