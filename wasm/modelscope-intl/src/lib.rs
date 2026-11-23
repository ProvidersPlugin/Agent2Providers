use agent2api_wasm_sdk::{self as sdk, Result, Value};

// 两地区只共用编译期实现；本包的身份、网络权限、账号与目录均独立。
#[path = "../../modelscope/src/provider.rs"]
mod provider;

sdk::export_provider!(handle);

fn handle(method: &str, params: Value) -> Result<Value> {
    provider::handle(provider::Region::Intl, method, params)
}
