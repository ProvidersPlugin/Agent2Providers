use agent2api_wasm_sdk::{self as sdk, Result, Value};

mod provider;

sdk::export_provider!(handle);

fn handle(method: &str, params: Value) -> Result<Value> {
    provider::handle(provider::Region::Cn, method, params)
}
