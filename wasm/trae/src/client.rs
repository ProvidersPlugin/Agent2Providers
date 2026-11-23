//! SDK HTTP薄层；受控origin与所有业务信封在本包，不接受任意URL/请求头覆盖。
//! 登录与刷新不记录请求，传输错误只用固定脱敏文案。

use super::errors;
use agent2api_wasm_sdk::{http, Error, Result, Value};

pub struct Reply {
    pub status: u16,
    pub body: Value,
}

pub fn read(response: http::Response) -> Result<Reply> {
    let status = response.status;
    let bytes = response.bytes(4 * 1024 * 1024).map_err(errors::transport)?;
    let body = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) if !(200..300).contains(&status) => {
            return Err(errors::upstream(status, &Value::Null))
        }
        Err(_) => return Err(Error::http(502, "Trae 上游未返回有效 JSON")),
    };
    Ok(Reply { status, body })
}

pub fn post(url: &str, headers: Value, body: &Value) -> Result<Reply> {
    read(http::open("POST", url, headers, Some(body)).map_err(errors::transport)?)
}

impl Reply {
    pub fn accepted(self) -> Result<Value> {
        if !(200..300).contains(&self.status) || errors::failed(&self.body) {
            return Err(errors::upstream(self.status, &self.body));
        }
        if !self.body.is_object() {
            return Err(Error::http(502, "Trae 上游响应格式异常"));
        }
        Ok(self.body)
    }
}
