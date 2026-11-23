//! 本机导入只保存来源引用；展示不在无账号上下文的 account.view 中读取客户端。
use agent2api_wasm_sdk::{self as sdk, json, Result, Value};

use crate::{credentials::Credentials, endpoints, errors, util};

pub fn read() -> Result<Credentials> {
    let value = sdk::host("desktop.credentials.read", json!({})).map_err(errors::host)?;
    let credentials = Credentials::parse(&value)?;
    credentials.require_main()?;
    if [&credentials.uid, &credentials.login_name].iter()
        .any(|text| errors::contains_secret(text, &credentials.secrets())) {
        return Err(errors::invalid("CatPaw 本机用户标识包含受保护凭据，未保存或公开"));
    }
    Ok(credentials)
}

pub fn normalize(params: &Value) -> Result<Value> {
    let credentials = read()?;
    let name = params["name"].as_str().map(str::trim).filter(|name| !name.is_empty())
        .unwrap_or("CatPaw 本机实时关联");
    let secrets = credentials.secrets();
    let secret = json!({"id": endpoints::DESKTOP_ID, "source": "desktop", "desktop": true,
        "uid": credentials.uid, "loginName": credentials.login_name,
        "name": errors::public_text(name, &secrets, 100)});
    // 同一次读取构造引用和公开形态，避免客户端恰好切号时产生两份不同身份。
    let mut public = public(&secret)?;
    public["hasToken"] = json!(true);
    public["available"] = json!(true);
    public["validation"] = json!("desktop_live");
    Ok(json!({"identity": endpoints::DESKTOP_ID, "secret": secret,
        "public": public, "desktop": true}))
}

pub fn public(secret: &Value) -> Result<Value> {
    let secret = secret.get("pluginSecret").filter(|value| value.is_object())
        .or_else(|| secret.get("secret").filter(|value| value.is_object())).unwrap_or(secret);
    let protected = ["accessToken", "token", "access_token", "balanceToken", "refreshToken"]
        .iter().flat_map(|key| [secret[*key].as_str(), secret["auth"][*key].as_str()])
        .flatten().collect::<Vec<_>>();
    let name = secret["name"].as_str().filter(|name| !name.trim().is_empty())
        .unwrap_or("CatPaw 本机实时关联");
    // account.view 的宿主合同只有 secret，没有本次本机读取授权。未知不能写成 false，
    // 否则通用模型选择和积分刷新界面会将实际可用的本机来源排除。
    Ok(json!({"name": errors::public_text(name, &protected, 100),
        "uid": errors::public_text(&util::text(secret, &["uid"]), &protected, endpoints::MAX_USER_CHARS),
        "loginName": errors::public_text(&util::text(secret, &["loginName"]), &protected, endpoints::MAX_USER_CHARS),
        "tokenTail": "", "hasToken": false, "hasBalanceToken": false,
        "desktop": true, "source": "desktop", "available": null, "canRefresh": false,
        "migrationStage": "complete", "validation": "desktop_reference",
        "availabilityMessage": "本机实时关联只保存来源；使用时读取当前 CatPaw 登录态，未在静态展示时检查客户端。"}))
}

pub fn start(_: &Value) -> Result<Value> {
    Err(errors::invalid("CatPaw 本机导入应从添加账号入口提交，不使用网页等待流程"))
}

pub fn poll(_: &Value) -> Result<Value> {
    Err(errors::invalid("CatPaw 本机导入不使用网页等待流程；旧等待已结束，请从添加账号入口重新导入"))
}
