//! Zai 授权页面只能由本轮受控官方接口返回。
use agent2api_wasm_sdk::{login::managed, Error, Result};
use super::oauth::Vendor;

pub const READY: bool = true;

pub fn require_vendor(_: Vendor) -> Result<()> { Ok(()) }
pub fn require_services() -> Result<()> { managed::context().map(|_| ()) }

pub fn context(flow: &str, vendor: Vendor) -> Result<managed::Context> {
    let context = managed::context()?;
    let Vendor::Zai = vendor;
    if context.provider != super::endpoints::PROVIDER || context.flow != flow
        || context.method_index != 0
        || context.remaining_ms == 0 || context.expires_at <= agent2api_wasm_sdk::now()
        || !context.authorization_origins.is_empty() {
        return Err(Error::coded("主程序托管权限不属于当前国际版登录", "forbidden"));
    }
    Ok(context)
}

pub fn profile(vendor: Vendor) -> Result<&'static str> {
    let Vendor::Zai = vendor;
    Ok("autoclaw-intl.zai.v1")
}
