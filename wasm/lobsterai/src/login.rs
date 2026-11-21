//! 复用宿主 login.callback.v1 和已经存在的 /auth/callback；插件不创建本机服务器。
//! 授权门户只接受 HTTP 127.0.0.1:<port>/auth/callback，路径保持旧实现验证过的形状。

use agent2api_wasm_sdk::{self as sdk, json, state::Scope, Error, Result, Value};
use base64::{engine::general_purpose::STANDARD, Engine};
use url::Url;

use super::{account, upstream, version};

const KEY: &str = "authorization";
const TTL: i64 = 6 * 60 * 1000;

pub fn uuid() -> Result<String> {
    let value = sdk::host("random.bytes", json!({ "length": 16 }))?;
    let mut bytes = STANDARD
        .decode(value.as_str().unwrap_or(""))
        .map_err(|_| Error::new("安全随机数回应无效"))?;
    if bytes.len() != 16 {
        return Err(Error::new("安全随机数长度无效"));
    }
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

fn flow(params: &Value) -> Result<&str> {
    params["flow"]
        .as_str()
        .filter(|flow| {
            !flow.is_empty()
                && flow.len() <= 256
                && flow
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        })
        .ok_or_else(|| Error::http(400, "主程序未提供有效的登录流程"))
}

fn url(raw: &str) -> Result<Url> {
    if raw.is_empty()
        || raw.len() > 16 * 1024
        || raw
            .bytes()
            .any(|byte| byte <= b' ' || byte == 127 || byte == b'\\')
    {
        return Err(Error::http(400, "登录回调地址格式无效"));
    }
    let parsed = Url::parse(raw).map_err(|_| Error::http(400, "登录回调地址格式无效"))?;
    let authority = raw
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""))
        .unwrap_or("");
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || authority.contains('@')
        || parsed.fragment().is_some()
    {
        return Err(Error::http(400, "登录回调地址不能包含用户信息或片段"));
    }
    Ok(parsed)
}

fn redirect(params: &Value) -> Result<String> {
    // 只使用宿主注入的地址；payload 和表单里的 callback_url 一律不读取。
    let mut callback = url(params["callback_url"].as_str().unwrap_or(""))?;
    if callback.scheme() != "http"
        || callback.host_str() != Some("127.0.0.1")
        || callback.port().is_none_or(|port| port == 0)
        || callback.query().is_some()
    {
        return Err(Error::http(400, "主程序未提供可信的本机回调地址"));
    }
    callback.set_path("/auth/callback");
    Ok(callback.to_string())
}

pub fn start(params: &Value) -> Result<Value> {
    if params["payload"]["methodIndex"].as_u64().unwrap_or(0) != 0 {
        return Err(Error::http(400, "凭证导入请通过账号表单提交"));
    }
    let flow = flow(params)?;
    let redirect = redirect(params)?;
    let snapshot = sdk::state::get(Scope::Login, KEY, 1)?;
    if !snapshot.compatible {
        return Err(Error::new("登录状态版本不兼容，请重新发起"));
    }
    let locale = super::i18n::locale(params["payload"]["locale"].as_str().unwrap_or("zh-CN"));
    let state = json!({ "redirect_uri": redirect, "created_at": sdk::now(), "phase": "pending", "locale": locale });
    if !sdk::state::compare_exchange(Scope::Login, KEY, 1, snapshot.revision, state, TTL)? {
        return Err(Error::new("登录状态已变化，请重新发起"));
    }
    let url = format!(
        "{}/portal#/login?source=electron&redirect_uri={}&state={}",
        upstream::PORTAL,
        upstream::query(&redirect),
        upstream::query(flow)
    );
    Ok(json!({ "flow": flow, "url": url, "poll_interval_ms": 0,
        "instructions": super::i18n::login_text(&locale, "start"),
        "external_browser": false,
        "allowed_hosts": ["lobsterai.youdao.com", "126.com", "163.com", "weixin.qq.com", "open.weixin.qq.com", "127.0.0.1"],
        "login_ui": { "browserMode": "choice", "manualCallback": true, "callbackInput": "url",
            "captureCallback": true, "cookieCapture": null, "loopbackTargets": [redirect] } }))
}

fn active(value: &Value) -> bool {
    let age = sdk::now().saturating_sub(value["created_at"].as_i64().unwrap_or(0));
    value["phase"] == "pending" && (0..TTL).contains(&age)
}

enum Callback {
    Code(String),
    Denied,
}

fn callback_code(params: &Value, expected: &str) -> Result<Callback> {
    let callback = url(params["url"].as_str().unwrap_or(""))?;
    let expected = url(expected)?;
    if callback.scheme() != expected.scheme()
        || callback.host_str() != expected.host_str()
        || callback.port_or_known_default() != expected.port_or_known_default()
        || callback.path() != expected.path()
        || callback.path() != "/auth/callback"
        || params["url"].as_str().unwrap_or("").split('?').next() != Some(expected.as_str())
    {
        return Err(Error::http(400, "登录回调不属于本次本机接收地址"));
    }
    let pairs: Vec<_> = callback.query_pairs().collect();
    let states: Vec<_> = pairs.iter().filter(|(key, _)| key == "state").collect();
    let codes: Vec<_> = pairs.iter().filter(|(key, _)| key == "code").collect();
    let denied: Vec<_> = pairs.iter().filter(|(key, _)| key == "error").collect();
    if states.len() != 1 || states[0].1 != flow(params)? {
        return Err(Error::http(400, "登录回调不属于当前流程"));
    }
    if !denied.is_empty() {
        if denied.len() != 1
            || denied[0].1.trim().is_empty()
            || denied[0].1.len() > 1024
            || denied[0].1.chars().any(char::is_control)
        {
            return Err(Error::http(400, "授权错误回调格式无效"));
        }
        // 官方拒绝是终态；同时回显的授权码不得换取凭据或保留。
        return Ok(Callback::Denied);
    }
    if codes.len() != 1
        || codes[0].1.is_empty()
        || codes[0].1.len() > 8192
        || codes[0].1.bytes().any(|byte| byte <= b' ' || byte == 127)
    {
        return Err(Error::http(400, "登录回调必须包含唯一、有效的授权码"));
    }
    if params["code"]
        .as_str()
        .is_some_and(|code| !code.is_empty() && code != codes[0].1.as_ref())
    {
        return Err(Error::http(400, "回调授权码与当前请求不一致"));
    }
    Ok(Callback::Code(codes[0].1.to_string()))
}

pub fn callback(params: &Value) -> Result<Value> {
    flow(params)?;
    let snapshot = sdk::state::get(Scope::Login, KEY, 1)?;
    let locale =
        super::i18n::locale(snapshot.value["locale"].as_str().unwrap_or("zh-CN")).to_string();
    if !snapshot.compatible || !active(&snapshot.value) {
        return Ok(
            json!({ "state": "failed", "message": super::i18n::login_text(&locale, "ended") }),
        );
    }
    let code = match callback_code(
        params,
        snapshot.value["redirect_uri"].as_str().unwrap_or(""),
    ) {
        Ok(Callback::Code(code)) => code,
        Ok(Callback::Denied) => {
            if !sdk::state::compare_exchange(
                Scope::Login,
                KEY,
                1,
                snapshot.revision,
                json!({ "phase": "failed", "locale": locale }),
                TTL,
            )? {
                return Err(Error::http(409, "本次登录状态已变化"));
            }
            return Ok(
                json!({ "state": "failed", "message": super::i18n::login_text(&locale, "denied") }),
            );
        }
        Err(error) => {
            return Ok(json!({ "state": "pending", "retryable": true, "hint": error.message }))
        }
    };
    let mut consumed = snapshot.value;
    consumed["phase"] = json!("exchanging");
    // 先消耗 state 再换码；重复提交和不明结果不能重新使用一次性授权码。
    let exchanged = consumed.clone();
    if !sdk::state::compare_exchange(Scope::Login, KEY, 1, snapshot.revision, consumed, TTL)? {
        return Err(Error::http(409, "本次授权已在处理，请勿重复提交"));
    }
    let result = (|| -> Result<Value> {
        let keyfrom = upstream::OFFICIAL_KEYFROM;
        let uuid = uuid()?;
        let version = version::resolve();
        let body = json!({ "authCode": code, "uuid": uuid, "firstKeyfrom": keyfrom,
            "latestKeyfrom": keyfrom, "version": version });
        let value = upstream::anonymous("/api/auth/exchange", &body, &version)?;
        let data = upstream::envelope(&value, "授权换码")?;
        let secret = account::login_bundle(&data, &uuid, keyfrom)?;
        Ok(json!({ "state": "done", "account": account::normalized(secret)? }))
    })();
    // 不允许取消后的换码结果覆盖新状态；flow 内不保存令牌或授权码。
    let current = sdk::state::get(Scope::Login, KEY, 1)?;
    if !current.compatible
        || current.value != exchanged
        || !sdk::state::compare_exchange(
            Scope::Login,
            KEY,
            1,
            current.revision,
            json!({ "phase": if result.is_ok() { "done" } else { "failed" }, "locale": locale }),
            TTL,
        )?
    {
        return Ok(
            json!({ "state": "failed", "message": super::i18n::login_text(&locale, "ended") }),
        );
    }
    Ok(match result {
        Ok(value) => value,
        Err(error) => json!({ "state": "failed", "message": error.message }),
    })
}

pub fn poll(params: &Value) -> Result<Value> {
    flow(params)?;
    let state = sdk::state::get(Scope::Login, KEY, 1)?;
    let locale = super::i18n::locale(state.value["locale"].as_str().unwrap_or("zh-CN"));
    if state.compatible && (active(&state.value) || state.value["phase"] == "exchanging") {
        Ok(
            json!({ "state": "pending", "hint": super::i18n::login_text(&locale,
            if state.value["phase"] == "exchanging" { "processing" } else { "waiting" }) }),
        )
    } else {
        Ok(json!({ "state": "failed", "message": super::i18n::login_text(&locale, "ended") }))
    }
}

pub fn cancel() -> Result<Value> {
    let current = sdk::state::get(Scope::Login, KEY, 1)?;
    let _ =
        sdk::state::compare_exchange(Scope::Login, KEY, 1, current.revision, Value::Null, 1000)?;
    Ok(Value::Null)
}
