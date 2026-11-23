use agent2api_provider_protocol as proto;
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginCallbackResult {
    #[serde(flatten)]
    pub poll: proto::LoginPoll,
    /// 下一跳只通过受保护的回调响应返回，不混进公开进度文案。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_url: Option<String>,
    /// 输入不属于本轮登录时允许重新填写，不结束仍在等待的流程。
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub retryable: bool,
}

impl From<proto::LoginPoll> for LoginCallbackResult {
    fn from(poll: proto::LoginPoll) -> Self {
        Self {
            poll,
            next_url: None,
            retryable: false,
        }
    }
}
