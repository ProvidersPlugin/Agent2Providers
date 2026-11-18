//! 只允许已核对的 GET 链路。无 claim/confirm/OAuth/heartbeat 的发包入口。
//! http.timeout.v1 从宿主收到 open 起覆盖准备、连接及完整响应体，并与外层剩余预算取小。
//! 余额/活动状态 20 秒，目录 30 秒；期限不参与签名，不回落旧接口或重试。

use crate::{
    binding::Account,
    endpoints::{self, ReadRequest},
    errors, signer,
};
use agent2api_wasm_sdk::{http, Result, Value};

pub fn get(account: &Account, request: ReadRequest<'_>, maximum: usize) -> Result<Value> {
    account.unchanged()?;
    account.readable()?;
    let timeout_ms = request.timeout_ms();
    let target = endpoints::target(request)?;
    if !target.domainless
        && errors::contains_secret(&account.credential.domain_id, &account.credential.secrets())
    {
        return Err(errors::invalid(
            "CodeArts 域身份与受保护凭据内容混用，未放入请求头或诊断记录",
        ));
    }
    let headers = signer::signed_get(&target, &account.credential, b"")?;
    let mut response = http::open_raw_timeout("GET", &target.url(), headers, None, timeout_ms)
        .map_err(errors::transport)?;
    let status = response.status;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.read().map_err(errors::transport)? {
        if bytes.len().saturating_add(chunk.len()) > maximum {
            return Err(errors::limited(
                "CodeArts 上游只读回应超过本地大小限制，未保存截断结果",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    account.unchanged()?;
    if status != 200 {
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        return Err(errors::classified(status, &value));
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| errors::protocol("CodeArts 上游回应不是完整 JSON，未保存或公开原始正文"))?;
    if errors::failure(&value) {
        return Err(errors::classified(status, &value));
    }
    if !value.is_object() {
        return Err(errors::protocol("CodeArts 上游回应缺少对象结构"));
    }
    Ok(value)
}
