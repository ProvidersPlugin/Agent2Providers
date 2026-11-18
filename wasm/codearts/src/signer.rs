//! 改写自本仓 codearts/signer.rs（AGPL-3.0-only，2026-11-10）。
//! 签入实际出站的固定协议头和最终 bytes；不签会被宿主覆盖的 UA。
//! Host 只取固定 URL authority，不接受 payload 自定 profile/Host 或签名逃生口。

use agent2api_wasm_sdk::{Result, Value};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::{credentials::Credential, encoding, endpoints::Target, errors, util};

pub fn signed_get(target: &Target, credential: &Credential, body: &[u8]) -> Result<Value> {
    if !body.is_empty() { return Err(errors::invalid("CodeArts GET 请求不接受正文")); }
    signed_request("GET", target, credential, body)
}

pub fn signed_request(method: &str, target: &Target, credential: &Credential, body: &[u8]) -> Result<Value> {
    if !matches!(method, "GET" | "POST" | "PUT") { return Err(errors::invalid("签名请求方法不支持")); }
    let mut headers = target.headers.clone();
    if headers.iter().any(|(name, _)| {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "authorization" | "user-agent" | "host"
        )
    }) {
        return Err(errors::invalid(
            "CodeArts 固定头集合含有不允许覆盖的签名字段",
        ));
    }
    if !credential.security_token.is_empty() {
        headers.push(("X-Security-Token".into(), credential.security_token.clone()));
    }
    if !target.domainless && !credential.domain_id.is_empty() {
        headers.push(("X-Domain-Id".into(), credential.domain_id.clone()));
    }
    let now = util::now()?;
    let date = chrono::DateTime::from_timestamp_millis(now)
        .ok_or_else(|| errors::protocol("CodeArts 签名时钟无效"))?
        .format("%Y%m%dT%H%M%SZ")
        .to_string();
    headers.push(("X-Sdk-Date".into(), date.clone()));
    if target.include_host {
        headers.push(("Host".into(), target.authority()?.to_string()));
    }
    let mut canonical: Vec<(String, String)> = headers
        .iter()
        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    canonical.sort_by(|left, right| left.0.cmp(&right.0));
    if canonical.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(errors::invalid("CodeArts 签名请求头重复"));
    }
    let names = canonical
        .iter()
        .map(|(key, _)| key.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let header_text = canonical
        .iter()
        .map(|(key, value)| format!("{key}:{value}\n"))
        .collect::<String>();
    let headers_for_signing = header_text;
    let uri = encoding::canonical_uri(&target.path)?;
    let query = encoding::canonical_query(&target.query);
    let payload_hash = util::hex(&Sha256::digest(body));
    let request = [
        method,
        &uri,
        &query,
        &headers_for_signing,
        &names,
        &payload_hash,
    ]
    .join("\n");
    let to_sign = format!(
        "SDK-HMAC-SHA256\n{date}\n{}",
        util::hex(&Sha256::digest(request.as_bytes()))
    );
    let mut mac = Hmac::<Sha256>::new_from_slice(credential.secret_access_key.as_bytes())
        .map_err(|_| errors::invalid("CodeArts 签名材料无效"))?;
    mac.update(to_sign.as_bytes());
    let signature = util::hex(&mac.finalize().into_bytes());
    headers.push((
        "Authorization".into(),
        format!(
            "SDK-HMAC-SHA256 Access={}, SignedHeaders={names}, Signature={signature}",
            credential.access_key_id
        ),
    ));
    Ok(Value::Object(
        headers
            .into_iter()
            .map(|(key, value)| (key, Value::String(value)))
            .collect(),
    ))
}
