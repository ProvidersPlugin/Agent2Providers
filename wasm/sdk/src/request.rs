use serde::Deserialize;

use crate::{host, json, Error, Result};

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct Context {
    pub client_stream: bool,
}

pub fn context() -> Result<Context> {
    serde_json::from_value(host("request.context", json!({}))?)
        .map_err(|_| Error::new("原始请求上下文格式无效"))
}
