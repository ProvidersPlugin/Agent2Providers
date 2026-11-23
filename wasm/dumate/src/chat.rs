use agent2api_wasm_sdk::{http, Error, Result, Value};

use crate::{account, endpoints, errors, network, request, stream::Output, token, util};

pub fn forward(params: Value) -> Result<()> {
    let request = request::build(params)?;
    let original = account::current()?;
    token::ensure(false)?;
    for attempt in 0..2 {
        let current = account::current()?;
        if current.identity != original.identity || current.cookie.header != original.cookie.header
            || current.cookie.origin != original.cookie.origin
        {
            return Err(errors::changed());
        }
        let token_value = util::text(&current.secret, &["appToken"]);
        if token_value.is_empty() {
            return Err(Error::http(401, "百度搭子缺少模型令牌，请刷新登录凭据"));
        }
        current.unchanged()?;
        let protected = current.protected();
        let response = http::open_timeout(
            "POST",
            &format!("{}{}", endpoints::GATEWAY, request.path),
            network::gateway_headers(&token_value, request.qianfan),
            Some(&request.body),
            600_000,
        )?;
        if !(200..300).contains(&response.status) {
            let error = network::read(response, &protected)
                .err()
                .unwrap_or_else(|| Error::http(502, "百度搭子对话请求失败"));
            if error.code.as_deref() == Some("dumate_session_expired") && attempt == 0 {
                token::ensure(true)?;
                continue;
            }
            return Err(error);
        }
        let mut output = Output::default();
        match output.consume(response, &protected, &current) {
            Ok(()) => { current.unchanged()?; return Ok(()); },
            Err(error)
                if !output.started
                    && error.code.as_deref() == Some("dumate_session_expired")
                    && attempt == 0 =>
            {
                token::ensure(true)?;
            }
            Err(error) => {
                output.fail(&error)?;
                return Err(error);
            }
        }
    }
    Err(Error::http(
        401,
        "百度搭子模型令牌换发后仍不可用，请重新登录",
    ))
}
