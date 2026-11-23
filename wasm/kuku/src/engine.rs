//! 原生engine.rs同一MD5签名，知识来自可核验0BSD kuku2api；不加载DLL。
//! 签名与发出的表单都使用原始字段串，因此先禁止&、+、%等表单歧义字符，
//! 不能对恶意Cookie拼出额外参数，也不能把编码后字段与签名输入混用。

use crate::{credentials::Credentials, endpoints, errors, network};
use agent2api_wasm_sdk::{json, Error, Result};
use md5::{Digest, Md5};

const KEY: &str = "34868b316de55815273b2616954b0286";
fn safe(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 8192
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_~./=".contains(&byte))
}
pub fn exchange(c: &Credentials) -> Result<String> {
    let bduss = c.get("BDUSS");
    let ptoken = c.ptoken();
    if !safe(bduss) || !safe(ptoken) {
        return Err(Error::http(
            400,
            "换发库库业务STOKEN需要有效BDUSS与PTOKEN，且不能包含表单分隔字符",
        ));
    }
    let fields = format!("appid=1&bduss={bduss}&ptoken={ptoken}&return_type=1&tpl=genflowpro&tpl_list=genflowpro|netdisk");
    let signature = format!(
        "{:x}",
        Md5::digest(format!("{fields}&sign_key={KEY}").as_bytes())
    );
    let body = format!("{fields}&sig={signature}");
    let headers = json!({ "User-Agent": endpoints::ENGINE_UA, "Referer": format!("{}/genflowpro", endpoints::BASE),
        "Origin": endpoints::BASE, "Accept": "application/json" });
    let value = network::form(endpoints::PASSPORT, headers, &body, &c.secrets())?;
    errors::errno(&value, &c.secrets())?;
    let stoken = value["stoken_list"]["genflowpro"]
        .as_str()
        .filter(|value| !value.is_empty() && value.len() <= 8192)
        .ok_or_else(|| Error::http(502, "库库换发响应缺少genflowpro STOKEN"))?;
    let mut checked = c.clone();
    checked.set_stoken(stoken)?;
    Ok(stoken.into())
}
