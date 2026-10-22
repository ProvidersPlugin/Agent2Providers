//! OfficeAce 余额：读订阅快照（`GET /v1/subscription`，V11 签名）并归一成
//! `adapter::ProviderAdapter::query_usage` 契约的形状（见 `subscription::summarize`）。
//!
//! 控制面临时凭据（AK/SK）缺失时回 `usage_not_configured`（400 + 中性标记）——
//! 手工只导入网关 Basic 凭据的账号就会走到这条，前端显示成「未配置查询」而不是失败。

use serde_json::Value;

use agent2api_provider_sdk::PluginCtx;

use crate::errors::GatewayError;

use super::{credentials, subscription};

/// 查询某 OfficeAce 账号的可领余额 / 积分。
pub async fn query_usage(ctx: &PluginCtx, account_id: &str) -> Result<Value, GatewayError> {
    let record = super::record_of(ctx, account_id).await?;
    let credential = credentials::from_record(Some(&record))?;
    let subscription = subscription::fetch_subscription(&credential).await?;
    Ok(subscription::summarize(&subscription))
}
