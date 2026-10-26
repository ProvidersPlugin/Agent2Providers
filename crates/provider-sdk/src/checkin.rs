use agent2api_provider_protocol as proto;
use serde::Serialize;

use crate::{PluginCtx, Provider};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckinResult {
    #[serde(flatten)]
    pub report: proto::CheckinReport,
    pub retry_at: Option<i64>,
}

impl From<proto::CheckinReport> for CheckinResult {
    fn from(report: proto::CheckinReport) -> Self {
        Self {
            report,
            retry_at: None,
        }
    }
}

pub(crate) async fn run(
    provider: &dyn Provider,
    ctx: &PluginCtx,
    account: &str,
    manual: bool,
) -> Result<CheckinResult, String> {
    let refreshable = provider.meta().capabilities.refresh;
    if refreshable {
        if provider.token_expiring(ctx, account).await? {
            provider.refresh_token(ctx, account).await?;
        } else {
            provider.ensure_token(ctx, account).await?;
        }
    }
    match provider.checkin_with_options(ctx, account, manual).await {
        Err(error) if refreshable && crate::errors::is_unauthorized(&error) => {
            provider.refresh_token(ctx, account).await?;
            provider.checkin_with_options(ctx, account, manual).await
        }
        result => result,
    }
}
