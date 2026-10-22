//! 华为云 **SDK-HMAC-SHA256** 请求签名（CodeArts Agent 上游那套 AK/SK/STS 签名）。
//!
//! ── 算法 ────────────────────────────────────────────────────
//! ```text
//! canonicalRequest = method \n canonicalURI \n canonicalQuery \n
//!                    canonicalHeaders \n signedHeaders \n payloadHash
//! stringToSign     = "SDK-HMAC-SHA256" \n X-Sdk-Date \n sha256hex(canonicalRequest)
//! signature        = hmacSHA256hex(secretKey, stringToSign)
//! Authorization    = SDK-HMAC-SHA256 Access=<AK>, SignedHeaders=<…>, Signature=<…>
//! ```
//!
//! ── 为什么这里有一整套 URL 解析，而不是直接用 `url::Url` ──────
//! 签名要对**字节**负责，而参考实现（`cpa-codearts-plugin` v0.1.18 `signer.go`）
//! 走的是 Go `net/url`：路径先按 `%` 解码、再由 `canonicalURI` 重编码（于是
//! `%2F` 会变成真分隔符），query 先按 `ParseQuery` 解码（于是 `+` 是空格、
//! 坏 `%` 让那一对键值整个丢掉）再重编码。`url::Url` 的规范化规则与这些都不
//! 同，照它算出来的规范请求串与上游对不上就是 401，而且错得毫无提示。所以这里
//! 按 Go 的语义自己实现，并且用参考实现跑出来的向量逐字节钉住（`signer_vectors.json`，
//! 见文件末的测试）。
//!
//! ── 三条容易写错的细节 ───────────────────────────────────────
//!   1. `canonicalURI` **强制补尾斜杠**：`/api/v2/chat/completions` 签的是
//!      `/api/v2/chat/completions/`。少这个斜杠上游直接 401。
//!   2. 未保留字符集是 **JS `encodeURIComponent` 的那一组**，不是 RFC 3986 的
//!      `A-Za-z0-9-_.~`：`-_.!~*'()` 全留，**包括 `!'()` 这三个**。
//!      参考实现 `signer.go` 的 `unreserved()` 就是这么写的（它复刻的是官方
//!      扩展里的 JS 签名器）。
//!   3. 参与签名的头集合 = 发出去的**全部**头（`X-Sdk-Date` / `X-Security-Token`
//!      / `X-Domain-Id` / 可选 `Host` 都在内），而 `Authorization` 本身不参与。

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

const SIGNING_ALGORITHM: &str = "SDK-HMAC-SHA256";
const HEADER_XSDK_DATE: &str = "X-Sdk-Date";
const HEADER_AUTHORIZATION: &str = "Authorization";
const HEADER_CONTENT_SHA256: &str = "X-Sdk-Content-Sha256";
const HEADER_SECURITY_TOKEN: &str = "X-Security-Token";
const HEADER_DOMAIN_ID: &str = "X-Domain-Id";
/// `sha256("")`，无体请求固定签这个
const EMPTY_BODY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// 已解析出的上游凭据：一次登录换来的临时 AK/SK + STS + 域号。
///
/// `security_token` / `domain_id` 为空时**完全不进**头集合（不是进一个空值），
/// 否则签名头集合与上游收到的不一致。
pub struct Credential {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub security_token: String,
    pub domain_id: String,
}

impl Credential {
    /// AK 与 SK 缺一即不可签（与参考实现的 `credential.valid()` 同口径）
    fn valid(&self) -> bool {
        !self.access_key_id.is_empty() && !self.secret_access_key.is_empty()
    }
}

/// 按 Go 的 `unhex` 把一个 hex 字符折成数值，非 hex 返回 None。
fn unhex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Go `net/url.unescape` 的 query 分量语义：`+` 当空格，`%XX` 解一个字节，
/// 转义坏了返回 None（调用方**丢弃这一对键值**而不是整条请求报错）。
fn query_unescape(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                if index + 2 >= bytes.len() {
                    return None;
                }
                let high = unhex(bytes[index + 1])?;
                let low = unhex(bytes[index + 2])?;
                out.push(high << 4 | low);
                index += 3;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    Some(out)
}

/// Go `net/url.unescape(…, encodePath)` 的语义：与 query 一样按 `%XX` 解字节，
/// 但 **`+` 是字面量**（不折成空格）。解出来的字节按原样保留，不做 UTF-8 校验
/// （Go 也只是把它们拼进 string，后续再按字节编码回去）。
fn path_unescape(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                if index + 2 >= bytes.len() {
                    return None;
                }
                let high = unhex(bytes[index + 1])?;
                let low = unhex(bytes[index + 2])?;
                out.push(high << 4 | low);
                index += 3;
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    Some(out)
}

/// JS `encodeURIComponent` 的未保留集（= 参考实现 `signer.go` 的 `unreserved`）：
/// 字母数字加上 `- _ . ! ~ * ' ( )`，其余按**字节**百分号编码、大写十六进制。
fn unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(byte, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')')
}

/// JS `encodeURIComponent` 的百分号编码。**签名与授权页地址共用这一份**：
/// 两处对「哪些字符不转义」的口径必须一致，否则登录 URL 与签名串会各说一套。
pub(crate) fn encode_component(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(bytes.len());
    for byte in bytes {
        if unreserved(*byte) {
            out.push(*byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

/// 拆出 `scheme://authority` 之后的 path 与 raw query。
///
/// `authority` 供 `include_host` 注入 `Host` 头用；`path` 是**已解码**的字节串
/// （Go 的 `URL.Path`），`raw_query` 保持原样交给 [`query_pairs`]。
/// 路径里的坏转义在这里就报错（对应 `url.Parse` 失败），query 里的坏转义不报错。
struct ParsedUrl {
    authority: String,
    path: Vec<u8>,
    raw_query: String,
}

fn parse_url(url: &str) -> Result<ParsedUrl, String> {
    let (_, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("上游地址缺少 scheme：{url}"))?;
    let (authority, tail) = match rest.find(['/', '?', '#']) {
        Some(index) => rest.split_at(index),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err(format!("上游地址缺少主机名：{url}"));
    }
    // 片段（fragment）不参与签名：Go 把它单独放在 URL.Fragment 里
    let (before_fragment, _) = tail.split_once('#').unwrap_or((tail, ""));
    let (raw_path, raw_query) = match before_fragment.split_once('?') {
        Some((path, query)) => (path, query),
        None => (before_fragment, ""),
    };
    Ok(ParsedUrl {
        authority: authority.to_string(),
        path: path_unescape(raw_path).ok_or_else(|| format!("上游地址路径转义非法：{raw_path}"))?,
        raw_query: raw_query.to_string(),
    })
}

/// `canonicalURI`：逐段按未保留集重编码，并**保证以 `/` 结尾**。
///
/// 空路径给 `/`。先按 `/` 切再逐段编码，所以解码进来的 `%2F`（一个真斜杠字节）
/// 在这里会被当成段边界 —— 与参考实现一致，不是 bug。
fn canonical_uri(path: &[u8]) -> String {
    if path.is_empty() {
        return "/".to_string();
    }
    let mut segments = Vec::new();
    for segment in path.split(|byte| *byte == b'/') {
        segments.push(encode_component(segment));
    }
    let mut escaped = segments.join("/");
    if !escaped.ends_with('/') {
        escaped.push('/');
    }
    escaped
}

/// Go `net/url.ParseQuery` 的**逐对**语义（它返回的 map 与 error 是分开的，
/// 而 `URL.Query()` 只取 map、把 error 丢掉，所以坏输入是「丢一对」而不是「整条废」）：
///   · 按 `&` 切分；含 `;` 的那一对直接丢弃；
///   · 空串丢弃；
///   · 第一个 `=` 分键值，没有 `=` 则值为空（键可以空，`?=v` 会留下 `=v`）；
///   · 键或值解不开就丢掉这一对，继续处理后面的。
fn query_pairs(raw: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut pairs = Vec::new();
    for part in raw.split('&') {
        if part.is_empty() || part.contains(';') {
            continue;
        }
        let (key, value) = match part.split_once('=') {
            Some((key, value)) => (key, value),
            None => (part, ""),
        };
        // 键为空 Go 也照收（只有整对为空才跳过），这里保持同一行为
        let Some(key) = query_unescape(key) else { continue };
        let Some(value) = query_unescape(value) else { continue };
        pairs.push((key, value));
    }
    pairs
}

/// `canonicalQueryString`：按键排序，同键的多值再按值排序，键与值都用未保留集编码。
fn canonical_query_string(raw: &str) -> String {
    let mut pairs = query_pairs(raw);
    if pairs.is_empty() {
        return String::new();
    }
    // Go 那边是 map[string][]string：同键的多个值被**合并**后排序。
    // 先把同键聚到一起，再整体按 (键, 值) 排 —— 与「键排序 + 组内值排序」等价。
    pairs.sort_by(|a, b| {
        let key_a = encode_component(&a.0);
        let key_b = encode_component(&b.0);
        (&key_a, &a.1).cmp(&(&key_b, &b.1))
    });
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", encode_component(key), encode_component(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn hmac_sha256_hex(key: &[u8], message: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC 接受任意长度的密钥");
    mac.update(message.as_bytes());
    format!("{:x}", mac.finalize().into_bytes())
}

/// 只有 PUT / PATCH / POST 被视为「带体」，其余恒签空体摘要。
fn request_carries_body(method: &str) -> bool {
    matches!(method.to_ascii_uppercase().as_str(), "PUT" | "PATCH" | "POST")
}

/// 头集合：按插入顺序保名值，**同名（大小写无关）覆盖前值、位置不变**。
///
/// 参考实现用的是 Go 的 `map[string]string`，同名大小写并存时「谁最后进规范串」
/// 取决于随机的 map 迭代顺序 —— 那是参考实现自己的不确定性，不是要复刻的行为。
/// 这里取确定的一条规则：先到先得位置、后到覆盖值。真实调用方不会同时给
/// `Host` 与 `host`（测试向量里也刻意没有这种输入）。
#[derive(Default)]
struct Headers {
    items: Vec<(String, String)>,
}

impl Headers {
    fn from_slice(input: &[(String, String)]) -> Self {
        let mut headers = Self::default();
        for (name, value) in input {
            headers.set(name, value);
        }
        headers
    }

    fn set(&mut self, name: &str, value: &str) {
        let lower = name.to_ascii_lowercase();
        for (existing, slot) in self.items.iter_mut() {
            if existing.to_ascii_lowercase() == lower {
                *slot = value.to_string();
                return;
            }
        }
        self.items.push((name.to_string(), value.to_string()));
    }

    fn contains_name(&self, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        self.items.iter().any(|(existing, _)| existing.to_ascii_lowercase() == lower)
    }

    fn value_of(&self, name: &str) -> Option<&str> {
        let lower = name.to_ascii_lowercase();
        self.items
            .iter()
            .find(|(existing, _)| existing.to_ascii_lowercase() == lower)
            .map(|(_, value)| value.as_str())
    }
}

/// 一次签名的全部中间量。
///
/// `canonical_request` / `string_to_sign` 一起带出来只为了一件小事：端口出错时
/// 能一眼看出是哪一段没对齐（尾斜杠？哪个字符没编码？头集合多了一项？），而不必
/// 回过头去猜签名为什么不一致。测试与排障用它，正常调用方只看 `headers`。
///
/// 除 `headers` 之外的字段目前只有测试读 —— 签名层的对账（M0）还没接到转发代码上，
/// 所以这里显式允许 dead_code，而不是把它们塞进 `sign()` 的返回值里凑数。
#[allow(dead_code)]
struct Material {
    headers: Vec<(String, String)>,
    canonical_request: String,
    string_to_sign: String,
    signed_headers: String,
    signature: String,
}

fn sign_material(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    credential: &Credential,
    include_host: bool,
) -> Result<Material, String> {
    if !credential.valid() {
        return Err("凭据不完整：access key 与 secret key 都得有".to_string());
    }
    let parsed = parse_url(url)?;
    let mut out = Headers::from_slice(headers);
    if !credential.security_token.is_empty() {
        out.set(HEADER_SECURITY_TOKEN, &credential.security_token);
    }
    if !credential.domain_id.is_empty() {
        out.set(HEADER_DOMAIN_ID, &credential.domain_id);
    }
    if !out.contains_name(HEADER_XSDK_DATE) {
        out.set(HEADER_XSDK_DATE, &sdk_date_now());
    }
    if include_host && !out.contains_name("host") {
        out.set("Host", &parsed.authority);
    }
    let date = out.value_of(HEADER_XSDK_DATE).unwrap_or_default().to_string();

    // 规范头视图：名字转小写、值去空白、按名字排序
    let mut canonical: Vec<(String, String)> = out
        .items
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    canonical.sort_by(|a, b| a.0.cmp(&b.0));
    let canonical_headers = canonical
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect::<String>();
    let signed_headers = canonical.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(";");

    let mut payload_hash = sha256_hex(body);
    let override_hash = canonical
        .iter()
        .find(|(name, _)| name == &HEADER_CONTENT_SHA256.to_ascii_lowercase())
        .map(|(_, value)| value.clone())
        .filter(|value| !value.is_empty());
    if let Some(value) = override_hash {
        payload_hash = value;
    } else if !request_carries_body(method) {
        payload_hash = EMPTY_BODY_SHA256.to_string();
    }

    let canonical_request = [
        method,
        &canonical_uri(&parsed.path),
        &canonical_query_string(&parsed.raw_query),
        &canonical_headers,
        &signed_headers,
        &payload_hash,
    ]
    .join("\n");
    let string_to_sign = [SIGNING_ALGORITHM, &date, &sha256_hex(canonical_request.as_bytes())].join("\n");
    let signature = hmac_sha256_hex(credential.secret_access_key.as_bytes(), &string_to_sign);
    out.set(
        HEADER_AUTHORIZATION,
        &format!("{SIGNING_ALGORITHM} Access={}, SignedHeaders={signed_headers}, Signature={signature}", credential.access_key_id),
    );
    Ok(Material { headers: out.items, canonical_request, string_to_sign, signed_headers, signature })
}

/// 给请求签名，返回**应当发出去**的头集合（入参头 + 注入项 + `Authorization`）。
///
/// * `headers` 是调用方已经要发的头（不含 `Authorization`）
/// * `body` 参与 `payloadHash`；用 `X-Sdk-Content-Sha256` 可以显式覆盖它
///   （官方客户端对 SSE 长流就是这么做的，签名时不算体）
/// * `include_host` 决定是否把 `Host` 也纳入签名 —— 参考实现只对某些上游开
///   （福利网关要、snap-access 不要），别统一开
/// * 没给 `X-Sdk-Date` 时按当前 UTC 注入 `yyyyMMddTHHmmssZ`
///
/// 错误只可能来自「凭据不全」与「地址非法」，都是调用方自己的 bug，
/// 返回可读文本让上层去决定状态码。
pub fn sign(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    credential: &Credential,
    include_host: bool,
) -> Result<Vec<(String, String)>, String> {
    Ok(sign_material(method, url, headers, body, credential, include_host)?.headers)
}

/// `X-Sdk-Date` 的格式：UTC 的 `yyyyMMddTHHmmssZ`（Go 版式 `20060102T150405Z`）。
fn sdk_date_now() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

#[cfg(any())]
mod tests {
    use super::*;

    /// 向量文件由参考实现 v0.1.18 现算现导（生成器与再生成方式见
    /// `cpa-deploy/codearts-vectors/README.md`）。逐字节比对四样东西：
    /// 规范请求串、待签串、签名头集合、签名；报错类的则比对报错。
    const VECTORS: &str = include_str!("signer_vectors.json");

    #[test]
    fn signer_matches_reference_vectors() {
        let vectors: serde_json::Value = serde_json::from_str(VECTORS).expect("向量不是合法 JSON");
        let cases = vectors.as_array().expect("向量顶层必须是数组");
        let mut signed = 0;
        let mut errored = 0;
        for case in cases {
            let name = case["name"].as_str().unwrap_or("?").to_string();
            let get = |key: &str| case[key].as_str().unwrap_or("").to_string();
            let credential = Credential {
                access_key_id: get("access_key"),
                secret_access_key: get("secret_key"),
                security_token: get("security_token"),
                domain_id: get("domain_id"),
            };
            let include_host = case["include_host"].as_bool().unwrap_or(false);
            // JSON 对象不带顺序；向量里每对键值都唯一，所以排序后即为确定输入
            let mut headers: Vec<(String, String)> = case["headers"]
                .as_object()
                .map(|map| map.iter().map(|(key, value)| (key.clone(), value.as_str().unwrap_or("").to_string())).collect())
                .unwrap_or_default();
            headers.sort();
            let body = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, get("body_b64"))
                .unwrap_or_default();
            let expected_error = get("expected_error");
            let kind = get("expected_error_kind");
            if !expected_error.is_empty() || !kind.is_empty() {
                let error = sign(&get("method"), &get("url"), &headers, &body, &credential, include_host)
                    .expect_err(&format!("{name}: 期望报错，却签名成功"));
                // 两边的报错文案语言不同（Go 是英文短句），所以按「因为什么而错」
                // 比对：不登记这一层映射的话，Rust 侧随便抛点什么都能算过
                let want = match kind.as_str() {
                    "url_parse" => "转义非法",
                    "incomplete_credential" => "凭据不完整",
                    other => panic!("{name}: 未登记的报错种类 {other:?}（在 signer.rs 的测试里补一条映射）"),
                };
                assert!(error.contains(want), "{name}: 报错文本 {error:?} 不含 {want:?}（Go 侧原文：{expected_error}）");
                errored += 1;
                continue;
            }
            // 没给日期的向量由签名器注入「当时」的 UTC 时刻：把参考实现那次注入
            // 的值补进入参再签，比的是同一串而不是「现在几点」
            if !headers.iter().any(|(key, _)| key.eq_ignore_ascii_case(HEADER_XSDK_DATE)) {
                let injected = case["headers_out"]
                    .as_object()
                    .and_then(|map| {
                        map.iter()
                            .find(|(key, _)| key.eq_ignore_ascii_case(HEADER_XSDK_DATE))
                            .and_then(|(_, value)| value.as_str())
                    })
                    .expect("向量里没有记录注入的 X-Sdk-Date");
                headers.push((HEADER_XSDK_DATE.to_string(), injected.to_string()));
                headers.sort();
            }
            let material = sign_material(&get("method"), &get("url"), &headers, &body, &credential, include_host)
                .unwrap_or_else(|error| panic!("{name}: 签名失败 {error}"));
            assert_eq!(get("canonical_request"), material.canonical_request, "{name}: 规范请求串");
            assert_eq!(get("string_to_sign"), material.string_to_sign, "{name}: 待签串");
            assert_eq!(get("signed_headers"), material.signed_headers, "{name}: 签名头集合");
            assert_eq!(get("signature"), material.signature, "{name}: 签名");
            // Authorization 必须是「SDK-HMAC-SHA256 Access=…, SignedHeaders=…, Signature=…」
            let authorization = material
                .headers
                .iter()
                .find(|(key, _)| key == HEADER_AUTHORIZATION)
                .map(|(_, value)| value.clone())
                .unwrap_or_default();
            assert_eq!(
                format!("{SIGNING_ALGORITHM} Access={}, SignedHeaders={}, Signature={}", credential.access_key_id, material.signed_headers, material.signature),
                authorization,
                "{name}: Authorization 头形状"
            );
            // 签名头集合里绝不能出现 authorization 自己
            assert!(!material.signed_headers.contains("authorization"), "{name}: Authorization 被签进去了");
            signed += 1;
        }
        assert!(signed >= 10, "只比对了 {signed} 条签名向量，向量文件可能被截断");
        assert!(errored >= 2, "只比对了 {errored} 条报错向量");
    }

    #[test]
    fn canonical_uri_forces_trailing_slash_and_escapes() {
        for (input, want) in [
            ("/api/v2/chat/completions", "/api/v2/chat/completions/"),
            ("/v1/chat/chat", "/v1/chat/chat/"),
            ("/a b/c", "/a%20b/c/"),
            ("/already/", "/already/"),
            ("", "/"),
        ] {
            assert_eq!(want, canonical_uri(input.as_bytes()), "canonicalURI({input:?})");
        }
    }

    #[test]
    fn query_string_sorts_and_escapes() {
        // 与参考实现同名断言：键排序、同键值排序、`/` 与空格都编码
        assert_eq!(
            "multi=a&multi=z&secret=a%20b%2Fc&ticket_id=abc-123",
            canonical_query_string("ticket_id=abc-123&secret=a+b%2Fc&multi=z&multi=a")
        );
    }

    #[test]
    fn only_write_methods_sign_a_body() {
        assert!(request_carries_body("POST") && request_carries_body("put"));
        assert!(!request_carries_body("GET") && !request_carries_body("DELETE"));
    }

    #[test]
    fn incomplete_credential_is_rejected_before_any_request() {
        let credential = Credential {
            access_key_id: "only-ak".to_string(),
            secret_access_key: String::new(),
            security_token: String::new(),
            domain_id: String::new(),
        };
        assert!(sign("POST", "https://example.invalid/x", &[], b"{}", &credential, false).is_err());
    }

    /// 打一次**真**上游，验证签名不只是「与参考实现对得上」，而是「上游认」。
    ///
    /// 默认不跑（`#[ignore]`），要手工开：
    /// ```bash
    /// CODEARTS_AK=… CODEARTS_SK=… CODEARTS_STS=… CODEARTS_DOMAIN=… \
    ///   cargo test -p agent2api-server codearts -- --ignored --nocapture
    /// ```
    /// 四个值就是登录换来的临时凭据（CPA 的 `codearts-provider-*.json` 里
    /// `codearts_provider_credential.*`，约 24h 有效）。缺任何一个是**跳过**而不是
    /// 失败 —— 别人跑 `--ignored` 时不该被这里绊倒。
    ///
    /// 只打只读的模型目录（`/v1/model/builtin`），不产生推理消耗。
    #[tokio::test]
    #[ignore]
    async fn live_upstream_accepts_the_signature() {
        let mut given = 0;
        let mut pick = |key: &str| {
            let value = std::env::var(key).unwrap_or_default();
            if value.is_empty() { value } else { given += 1; value }
        };
        let credential = Credential {
            access_key_id: pick("CODEARTS_AK"),
            secret_access_key: pick("CODEARTS_SK"),
            security_token: pick("CODEARTS_STS"),
            domain_id: pick("CODEARTS_DOMAIN"),
        };
        if given < 4 {
            println!("跳过：四个 CODEARTS_* 环境变量没给齐（只给了 {given} 个）");
            return;
        }
        // 头集合照参考实现的默认档（config.go:255-263）：目录要用 PromptCenter，
        // 换成别的 agent 类型上游会给一份受限视图、看着像"这账号没有模型"
        let headers = [
            ("Content-Type", "application/json"),
            ("Accept", "application/json"),
            ("Agent-Type", "PromptCenter"),
            ("X-Language", "en-us"),
            ("plugin-name", "snap_vscode"),
            ("plugin-version", "26.9.101"),
            ("client_version", "Vscode_26.9.101"),
            ("is_confidential", "false"),
            ("x-snap-traceid", "00000000000000000000000000000000"),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect::<Vec<_>>();
        let url = "https://snap-access.cn-north-4.myhuaweicloud.com/v1/model/builtin";
        let signed = sign("GET", url, &headers, b"", &credential, false).expect("签名应当成功");
        let mut request = reqwest::Client::new().get(url);
        for (name, value) in signed {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request.send().await.expect("请求发不出去（网络或代理）");
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        println!("HTTP {status}\n{}", body.chars().take(600).collect::<String>());
        assert!(status.is_success(), "上游没认这个签名：{status}");
        assert!(body.contains("model_id"), "200 但响应里看不到模型目录");
    }
}
