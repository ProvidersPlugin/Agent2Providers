use agent2api_wasm_sdk::{self as sdk, http, Error, Result, Value};

use super::{account, config::Config, errors, fingerprint, request, stream::Output};

pub fn forward(params: &Value) -> Result<()> {
    let record = sdk::account()?;
    let key = account::api_key(&record["secret"])?;
    let config = Config::load()?;
    let request = request::build(&key, &params["body"], &params["headers"], &config)?;
    fingerprint::ensure_initialized(&key, &config)?;
    account::unchanged(&record)?;
    let mut response = http::open("POST", &request.url, request.headers, Some(&request.body))?;
    account::unchanged(&record)?;
    if !(200..300).contains(&response.status) {
        let status = response.status;
        let bytes = response.bytes(1024 * 1024)?;
        let body: Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        let mut error = errors::http_error(status, &body);
        errors::redact(&mut error, &key);
        return Err(error);
    }
    if response.headers["content-type"]
        .as_str()
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/html"))
    {
        return Err(
            Error::http(502, "Command Code 返回了网页而不是模型回答").with_code("protocol_error")
        );
    }
    let mut output = Output::new(request.model)?;
    let result = (|| -> Result<()> {
        while let Some(bytes) = response.read()? {
            account::unchanged(&record)?;
            output.push(&bytes)?;
            if output.finished {
                return Ok(());
            }
        }
        account::unchanged(&record)?;
        output.finish()
    })();
    match result {
        Ok(()) => Ok(()),
        Err(mut error) => {
            errors::redact(&mut error, &key);
            if error.status.is_none() {
                error.status = match error.code.as_deref() {
                    Some("forbidden") => None,
                    Some("timeout" | "network_timeout" | "upstream_timeout") => Some(504),
                    Some("aborted" | "ABORT_ERR" | "cancelled") => Some(499),
                    _ => Some(502),
                };
            }
            // 只在已经开始输出后补错误帧。首帧前直接返回结构化错误，留给宿主分类/轮换。
            // 中断不伪造 finish_reason、usage 或 [DONE]，也不在插件内从头重发生成。
            if output.started && !output.finished {
                sdk::stream_event(&errors::error_event(&error))?;
            }
            Err(error)
        }
    }
}
