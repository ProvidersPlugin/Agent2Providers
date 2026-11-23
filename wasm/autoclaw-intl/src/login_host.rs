//! Google 使用固定来源，Zai 页面只能由本轮受控官方接口返回。
use agent2api_wasm_sdk::{login::managed, Error, Result};
use super::oauth::Vendor;

pub const READY: bool = true;

pub fn require_vendor(_: Vendor) -> Result<()> { Ok(()) }
pub fn require_services() -> Result<()> { managed::context().map(|_| ()) }

pub fn context(flow: &str, vendor: Vendor) -> Result<managed::Context> {
    let context = managed::context()?;
    let origins_valid = match vendor {
        Vendor::Google => context.authorization_origins == ["https://accounts.google.com"],
        Vendor::Zai => context.authorization_origins.is_empty(),
    };
    if context.provider != super::endpoints::PROVIDER || context.flow != flow
        || context.method_index != match vendor { Vendor::Zai => 0, Vendor::Google => 1 }
        || context.remaining_ms == 0 || context.expires_at <= agent2api_wasm_sdk::now() || !origins_valid {
        return Err(Error::coded("主程序托管权限不属于当前国际版登录", "forbidden"));
    }
    Ok(context)
}

pub fn profile(vendor: Vendor) -> Result<&'static str> {
    Ok(match vendor { Vendor::Zai => "autoclaw-intl.zai.v1", Vendor::Google => "autoclaw-intl.google.v1" })
}
