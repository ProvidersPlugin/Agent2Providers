//! 原生授权query保留顺序，auth_callback_url不转义，PKCE为S256。
//! 授权码只向原variant对应的固定国内账号端点提交一次，任何结果不明均停止，不切备用源。
//! 完整OS/型号没有必填证据，DeviceName/Model/CPU/OSVersion留空；不发送假主机硬件。

use super::{client, credentials, device, endpoints, errors, headers, refresh, util};
use agent2api_wasm_sdk::{json, Error, Result, Value};
use sha2::{Digest, Sha256};

pub fn context(variant: &str, callback: &str) -> Result<Value> {
    let verifier = util::b64url(&util::random(48)?);
    let mut value = device::generate()?;
    value["verifier"] = json!(verifier);
    value["challenge"] = json!(util::b64url(&Sha256::digest(verifier.as_bytes())));
    value["trace"] = json!(util::uuid()?);
    value["machineId"] = json!(util::uuid()?);
    value["deviceId"] = json!(util::digits(16)?);
    value["platform"] = json!(headers::platform()?);
    value["callback"] = json!(callback);
    value["variant"] = json!(variant);
    Ok(value)
}

fn encode(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(b))
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn guidance(current: impl Fn() -> Result<()>) -> Result<String> {
    for host in endpoints::ACCOUNT_ORIGINS {
        current()?;
        let reply = client::post(
            &format!("{host}{}", endpoints::GUIDANCE),
            headers::oauth(None),
            &json!({ "loginTraceID": "", "login_trace_id": "" }),
        );
        let Ok(reply) = reply else {
            continue;
        };
        let Ok(value) = reply.accepted() else {
            continue;
        };
        for layer in [
            Some(&value),
            value.get("Result"),
            value.get("result"),
            value.get("data"),
            value.pointer("/data/Result"),
            value.pointer("/data/result"),
        ]
        .into_iter()
        .flatten()
        {
            let host = util::text(
                layer,
                &[
                    "LoginHost",
                    "loginHost",
                    "LoginURL",
                    "loginUrl",
                    "login_url",
                ],
            );
            if !host.is_empty() {
                return util::account_origin(&host);
            }
        }
    }
    Ok(endpoints::ACCOUNT.into())
}

pub fn authorization(host: &str, context: &Value) -> Result<String> {
    let host = util::account_origin(host)?;
    let variant = credentials::variant(context);
    let mut values: Vec<(&str, String, bool)> = vec![
        ("login_version", "1".into(), false),
        (
            "auth_from",
            if variant == "solo" { "solo" } else { "trae" }.into(),
            false,
        ),
        ("login_channel", "native_ide".into(), false),
        ("plugin_version", "1.0.0".into(), true),
        ("auth_type", "local".into(), false),
        ("client_id", endpoints::client_id(variant).into(), false),
        ("redirect", "0".into(), false),
        ("login_trace_id", util::text(context, &["trace"]), true),
        (
            "auth_callback_url",
            util::text(context, &["callback"]),
            false,
        ),
        ("machine_id", util::text(context, &["machineId"]), true),
        ("device_id", util::text(context, &["deviceId"]), true),
        ("x_device_id", util::text(context, &["deviceId"]), true),
        ("x_machine_id", util::text(context, &["machineId"]), true),
        ("x_device_type", util::text(context, &["platform"]), true),
        ("x_env", "prod".into(), true),
        ("x_app_version", endpoints::IDE_VERSION.into(), true),
        ("x_app_type", "trae".into(), true),
        ("code_challenge", util::text(context, &["challenge"]), true),
        ("code_challenge_method", "S256".into(), false),
    ];
    if variant == "solo" {
        values.push(("hide_saas_login", "true".into(), false));
    }
    Ok(format!(
        "{host}/authorization?{}",
        values
            .into_iter()
            .map(|(k, v, e)| format!("{k}={}", if e { encode(&v) } else { v }))
            .collect::<Vec<_>>()
            .join("&")
    ))
}

pub const CODE_EXCHANGE_STOP: &str = "trae_oauth_code_exchange_stopped";

/// 原variant/官方端点已在发起时确定；回调loginHost仅为回显，不授予改道或再次兑换权限。
/// 这是插件单次调用的防重发，不替代后续宿主原Call/租约/真实一次发送门。
pub fn exchange(
    context: &Value,
    code: &str,
    _login_host: &str,
    current: impl Fn() -> Result<()>,
) -> Result<Value> {
    let operation = || -> Result<Value> {
        let variant = match context["variant"].as_str() {
            Some("cn") => "cn",
            Some("solo") => "solo",
            _ => {
                return Err(Error::http(
                    409,
                    "Trae 本轮登录应用归属已失效，未猜测ClientID",
                ))
            }
        };
        if code.is_empty()
            || code.len() > 8192
            || code.trim() != code
            || code.chars().any(char::is_control)
        {
            return Err(Error::http(400, "Trae 本轮授权码格式无效"));
        }
        let public = util::text(context, &["devicePublicKey"]);
        let private = util::text(context, &["devicePrivateKey"]);
        device::validate(&public, &private)?;
        if public.is_empty() {
            return Err(Error::http(409, "Trae 本轮设备密钥丢失，请重新登录"));
        }
        let verifier = context["verifier"]
            .as_str()
            .filter(|value| {
                value.len() == 64
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
            })
            .ok_or_else(|| Error::http(409, "Trae 本轮PKCE材料已失效"))?;
        let body = json!({ "ClientID": endpoints::client_id(variant), "AuthCode": code, "CodeVerifier": verifier,
            "IDEVersion": endpoints::IDE_VERSION, "DeviceInfo": {
                "DeviceID": context["deviceId"], "MachineID": context["machineId"], "PlatformCode": endpoints::platform_code(variant),
                "DeviceType": "PC", "DeviceName": "", "DeviceModel": "", "ClientVersion": endpoints::IDE_VERSION,
                "DevicePublicKey": public, "DeviceBrand": "", "DeviceCPU": "", "OSInfo": context["platform"], "OSVersion": ""
            } });
        current()?;
        // 两个国内variant共用原候选首个官方账号源，只是ClientID不同；没有备用HTTP循环。
        let reply = client::post(
            &format!("{}{}", endpoints::ACCOUNT, endpoints::EXCHANGE),
            headers::oauth(Some("")),
            &body,
        )?;
        current()?;
        if !(200..300).contains(&reply.status) || errors::failed(&reply.body) {
            return Err(errors::upstream(reply.status, &reply.body));
        }
        refresh::token_fields(&reply.body)
    };
    operation().map_err(|error| {
        let mut stopped = Error::http(
            error
                .status
                .filter(|status| (400..600).contains(status))
                .unwrap_or(502),
            "Trae 本轮授权码兑换失败或结果未确认，已停止；不会向备用地址重兑，请明确重新发起授权",
        )
        .with_code(CODE_EXCHANGE_STOP);
        stopped.upstream_code = error.upstream_code;
        // 不保留token_expired/credential_scope；授权码错误不能触发账号刷新或同码重试。
        stopped
    })
}

/// 不以HTTP状态或上游业务码覆盖本地“本轮不能重发”的结论。
pub fn stops_retry(body: &Value) -> bool {
    [
        body.get("providerCode"),
        body.get("code"),
        body.pointer("/error/providerCode"),
        body.pointer("/error/code"),
    ]
    .into_iter()
    .flatten()
    .any(|value| value.as_str() == Some(CODE_EXCHANGE_STOP))
}
