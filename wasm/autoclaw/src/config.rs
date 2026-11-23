//! 本家只读配置按 provider 命名空间隔离；不允许环境变量或自定义域名旁路。
use agent2api_wasm_sdk::{self as sdk, json, Error, Result, Value};
use super::{models, util};

pub fn default_route() -> Result<String> {
    let value = sdk::settings::get("defaultRoute")?;
    let route = value.as_str().map(str::trim).filter(|value| !value.is_empty()).unwrap_or(models::DEFAULT_ROUTE);
    if !util::printable(route, 256) || !models::route_shape(route) {
        return Err(Error::http(400, "AutoClaw 默认模型路由格式无效"));
    }
    Ok(route.into())
}
pub fn strip_newlines() -> Result<bool> { Ok(sdk::settings::get("stripNewlineKeepalive")?.as_bool().unwrap_or(false)) }
pub fn schema() -> Value {
    json!({ "version": 1, "title": "AutoClaw 国内版", "runtime_overrides": false,
        "description": "独立国内设置；不读取环境变量，默认保留上游正文换行。",
        "fields": [
            { "key": "defaultRoute", "label": "未知模型的默认路由", "type": "string", "default": models::DEFAULT_ROUTE,
                "trim": true, "empty_is_default": true, "max_length": 256 },
            { "key": "stripNewlineKeepalive", "label": "清理仅含换行的保活帧", "type": "boolean", "default": false }
        ] })
}
