//! 小浣熊 1.1.5 的组织身份取自同一份凭证；个人身份不发送 X-Org-Code。

use serde_json::Value;

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
