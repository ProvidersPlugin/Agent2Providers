//! 通用 Anthropic 出站转换，不包含提供商的端点、鉴权或套餐配置。

#[path = "providers/zcode/anthropic_outbound.rs"]
pub mod anthropic_outbound;
#[path = "providers/zcode/anthropic_shared.rs"]
pub mod anthropic_shared;
#[path = "providers/zcode/native_tool.rs"]
pub mod native_tool;
