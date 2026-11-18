//! 只服务显式source.kind=desktop的一次性导入；普通凭据失效不暗读本机。
//! 来源依据原生accounts.rs：已知数据根的userdata/astron-session.json，或acode配置的
//! [model_providers.astron-spark]单节；根目录查找由主程序固定实现，不接收调用方路径。
//! helper协议：read/{} -> {credentials:{key?,modelBearerToken?,uid?,token?,accountId?,ssoSessionId?,banned?}}。
//! 不回整份session/config、路径、其它provider/频道秘密；无提供者就明确不可用。

use super::{account, endpoints};
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};

pub fn read() -> Result<Value> {
    let reply = sdk::native_call(endpoints::DESKTOP_HELPER, "read", json!({})).map_err(|_| {
        Error::http(
            503,
            "AStudio 本机登录态不可读，请先在官方客户端登录，或改用短信、密码或上传凭据",
        )
        .with_code("native_helper_unavailable")
    })?;
    if !reply
        .as_object()
        .is_some_and(|object| object.len() == 1 && object.contains_key("credentials"))
    {
        return Err(Error::http(
            502,
            "AStudio 本机辅助返回的结果不是受限凭据投影",
        ));
    }
    let projected = &reply["credentials"];
    let allowed = [
        "key",
        "modelBearerToken",
        "uid",
        "token",
        "accountId",
        "ssoSessionId",
        "banned",
    ];
    if !projected
        .as_object()
        .is_some_and(|object| object.keys().all(|key| allowed.contains(&key.as_str())))
        || !["key", "modelBearerToken", "token", "ssoSessionId"]
            .iter()
            .any(|key| {
                projected[*key]
                    .as_str()
                    .is_some_and(|value| !value.trim().is_empty())
            })
    {
        return Err(Error::http(
            502,
            "AStudio 本机读取包含约定之外的字段或缺少登录凭据",
        ));
    }
    account::parse(projected, "desktop")
}
