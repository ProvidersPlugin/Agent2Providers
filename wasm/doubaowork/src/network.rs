//! 仅发送封闭的只读请求。保留原生版本、设备上下文，不隐式补本机设备号。
//! SDK 将账号代理与 UA 绑定到本次调用；失败不回退直连、不持久化 Set-Cookie。

use agent2api_wasm_sdk::{http, json, Result, Value};

use crate::{
    credentials::Credentials,
    endpoints::{self, ReadEndpoint},
    errors, util,
};

pub fn url(credentials: &Credentials, endpoint: ReadEndpoint) -> String {
    let mut query: Vec<(&str, String)> = if matches!(endpoint, ReadEndpoint::Identity) {
        vec![
            ("account_sdk_source", "web".into()),
            ("sdk_version", "2.2.5".into()),
            ("aid", "582478".into()),
            ("real_aid", "582478".into()),
            ("device_platform", "web".into()),
            ("samantha_web", "1".into()),
            ("use-olympus-account", "1".into()),
        ]
    } else {
        let version = version(credentials, endpoint);
        vec![
            ("aid", "1044603".into()),
            ("real_aid", "1044603".into()),
            ("device_platform", "web".into()),
            ("version_code", "20800".into()),
            ("samantha_web", "1".into()),
            ("web_platform", "desktop".into()),
            ("doubao_device_platform", "desktop".into()),
            ("client_platform", "pc_client".into()),
            ("pkg_type", "release_version".into()),
            (
                "runtime",
                "web".into(),
            ),
            ("runtime_version", "3.45.3".into()),
            ("pc_version", version.into()),
            ("doubao_pc_version", version.into()),
            ("use-olympus-account", "1".into()),
            ("language", "zh".into()),
            ("region", "CN".into()),
            ("sys_region", "CN".into()),
        ]
    };
    if !matches!(endpoint, ReadEndpoint::Identity) {
        let device = credentials.field("deviceId");
        if util::is_numeric_id(device) {
            query.push(("device_id", device.into()));
        }
        for (wire, field) in [
            ("iid", "iid"),
            ("web_id", "webId"),
            ("channel", "channel"),
            ("chromium_version", "chromiumVersion"),
        ] {
            let value = credentials.field(field);
            if !value.is_empty() {
                query.push((wire, value.into()));
            }
        }
        let fingerprint = credentials.field("fingerprint");
        if !fingerprint.is_empty() {
            query.push(("fp", fingerprint.into()));
        } else if util::is_numeric_id(device) {
            query.push(("fp", format!("verify_{device}")));
        }
        if endpoint.control() {
            for (wire, field) in [("tea_uuid", "teaUuid"), ("web_tab_id", "webTabId")] {
                let value = credentials.field(field);
                if !value.is_empty() {
                    query.push((wire, value.into()));
                }
            }
        }
    }
    let query = query
        .into_iter()
        .map(|(key, value)| format!("{}={}", util::encode(key), util::encode(&value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{}{}?{query}", endpoints::BASE, endpoint.path())
}

fn version(credentials: &Credentials, endpoint: ReadEndpoint) -> &str {
    let saved = credentials.field("clientVersion");
    if !saved.is_empty() {
        saved
    } else if endpoint.control() {
        endpoints::CONTROL_VERSION
    } else {
        endpoints::CLIENT_VERSION
    }
}

pub fn headers(credentials: &Credentials, endpoint: ReadEndpoint) -> Value {
    let saved = credentials.field("userAgent");
    let user_agent = if saved.is_empty() || saved == endpoints::USER_AGENT {
        if endpoint.control() {
            format!(
                "{} SamanthaDoubaoWork/{}",
                endpoints::USER_AGENT,
                version(credentials, endpoint)
            )
        } else {
            endpoints::USER_AGENT.into()
        }
    } else {
        saved.into()
    };
    let mut headers = json!({ "Cookie": credentials.cookie_header, "Origin": endpoints::BASE,
        "Referer": format!("{}{}", endpoints::BASE, if endpoint.ug() { "/pc/agent-explore" } else { "/chat/" }),
        "User-Agent": user_agent, "Agw-Js-Conv": "str", "Accept": "application/json, text/plain, */*" });
    if endpoint.method() == "POST" {
        headers["Content-Type"] = json!("application/json");
    }
    headers
}

pub fn read(
    credentials: &Credentials,
    endpoint: ReadEndpoint,
    body: Option<&Value>,
) -> Result<Value> {
    let response = http::open_timeout(
        endpoint.method(),
        &url(credentials, endpoint),
        headers(credentials, endpoint),
        body,
        30_000,
    )
    .map_err(errors::transport)?;
    let status = response.status;
    if (300..400).contains(&status) {
        return Err(errors::upstream(status, &Value::Null, endpoint));
    }
    let bytes = response
        .bytes(endpoints::MAX_RESPONSE)
        .map_err(errors::transport)?;
    let parsed = serde_json::from_slice::<Value>(&bytes);
    if !(200..300).contains(&status) {
        return Err(errors::upstream(
            status,
            parsed.as_ref().unwrap_or(&Value::Null),
            endpoint,
        ));
    }
    let value = parsed.map_err(|_| errors::protocol("豆包工作未返回有效 JSON，未回显原始响应"))?;
    if !value.is_object() {
        return Err(errors::protocol("豆包工作返回的只读响应不是对象"));
    }
    for field in ["code", "error_code"] {
        if let Some(raw) = value.get(field).filter(|value| !value.is_null()) {
            let code =
                util::integer(raw).ok_or_else(|| errors::protocol("豆包工作业务状态码格式无效"))?;
            if code != 0 && !(field == "code" && endpoint.ug() && code == 200) {
                return Err(errors::upstream(status, &value, endpoint));
            }
        }
    }
    let detail_error = value
        .get("error_detail")
        .filter(|value| value.is_object())
        .and_then(errors::business)
        .is_some_and(|code| code != 0);
    if value.get("success") == Some(&Value::Bool(false))
        || detail_error
        || value
            .get("error")
            .is_some_and(|error| !error.is_null() && error != &Value::Bool(false))
        || errors::verification(&value)
    {
        return Err(errors::upstream(status, &value, endpoint));
    }
    Ok(value)
}
