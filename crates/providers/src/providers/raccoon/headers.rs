//! 小浣熊 1.1.5 的请求头：组织身份、桌面端身份、请求 id 与外部覆盖。
//!
//! 组织身份取自同一份凭证；个人身份不发送 X-Org-Code。

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

/// 对话请求的额外头部文件（可选）。
///
/// 上游头部的取值（尤其 `X-Client-Version`）随客户端版本变化，写死在二进制里
/// 意味着每次对不上都要重新编译插件。这个文件存在时，**每条对话请求**都读一次：
/// 写成 `{"X-Client-Version": "v1.2.0"}` 这样的 JSON 对象即可覆盖同名头，
/// 其余键追加；改完文件下一条请求就生效。文件不存在 / 不是 JSON 对象 /
/// 值不是字符串时整体跳过，不影响正常转发。
const EXTERNAL_HEADERS_PATH: &str = "/var/lib/agent2api/raccoon-headers.json";

pub(super) fn org_code(record: &Value) -> String {
    let identity = crate::store::pick(record, &["officeIdentity", "office_identity"]);
    if identity == "personal" {
        String::new()
    } else {
        identity
    }
}

pub(super) fn append_org(
    headers: &mut Vec<(String, String)>,
    org_code: &str,
) -> Result<(), String> {
    if org_code.is_empty() || org_code == "personal" {
        return Ok(());
    }
    if !org_code.is_ascii() || org_code.bytes().any(|value| value < 32 || value == 127) {
        return Err("小浣熊组织身份无效，请重新导入登录态".to_string());
    }
    headers.push(("X-Org-Code".to_string(), org_code.to_string()));
    Ok(())
}

pub(super) fn desktop_client_headers() -> Vec<(String, String)> {
    let platform = if cfg!(target_os = "windows") {
        "desktop-windows"
    } else if cfg!(target_os = "macos") {
        "desktop-macos"
    } else {
        "desktop-linux"
    };
    vec![
        ("X-Client-Name".to_string(), "raccoon-work".to_string()),
        ("X-Client-Platform".to_string(), platform.to_string()),
        ("X-Client-Version".to_string(), "v1.1.5".to_string()),
    ]
}

/// 每请求唯一的 `X-Request-Id`，形态与官方 `createRequestID` 相同：
/// `relay_js_<毫秒时间戳>_<8 位十六进制>`。
///
/// 官方用 xorshift 生成短随机串；这里也是 xorshift64，种子混入纳秒时间戳与
/// 进程内单调计数器 —— 同一毫秒内的并发请求不会撞车，也不引入新依赖。
pub(super) fn request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let mut state = (now.as_nanos() as u64)
        ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ u64::from(std::process::id());
    if state == 0 {
        state = 1;
    }
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    format!("relay_js_{}_{:08x}", now.as_millis(), (state >> 32) as u32)
}

/// 合并外部头部文件：同名（大小写不敏感）覆盖，其余追加。
///
/// 每次请求都读，文件不存在时零开销地跳过 —— 见 [`EXTERNAL_HEADERS_PATH`]。
pub(super) fn append_external(headers: &mut Vec<(String, String)>) {
    let Ok(raw) = std::fs::read_to_string(EXTERNAL_HEADERS_PATH) else {
        return;
    };
    let Ok(Value::Object(object)) = serde_json::from_str::<Value>(&raw) else {
        return;
    };
    for (name, value) in object {
        let Some(value) = value.as_str() else {
            continue;
        };
        if !valid_header_name(&name) || !valid_header_value(value) {
            continue;
        }
        upsert(headers, name, value.to_string());
    }
}

/// HTTP 头名的合法字符集（RFC 9110 的 token：不含分隔符与控制字符）。
fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

/// HTTP 头值：可见 ASCII 与制表符；其余字符会让整条请求被 reqwest 拒绝。
fn valid_header_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
}

fn upsert(headers: &mut Vec<(String, String)>, name: String, value: String) {
    match headers
        .iter_mut()
        .find(|(key, _)| key.eq_ignore_ascii_case(name.as_str()))
    {
        Some((_, current)) => *current = value,
        None => headers.push((name, value)),
    }
}
