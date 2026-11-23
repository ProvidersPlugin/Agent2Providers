//! 只读端点是封闭集合：业务使用 POST 不等于可接入写接口。
//! 来源为当前原生 doubaowork/api、accounts、catalog、quota、rewards。

pub const PROVIDER: &str = "doubaowork";
pub const BASE: &str = "https://www.doubao.com";
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36";
pub const CLIENT_VERSION: &str = "2.33.5";
pub const CONTROL_VERSION: &str = "2.36.11";
pub const MAX_RESPONSE: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy)]
pub enum ReadEndpoint {
    Identity,
    UserLaunch,
    Catalog,
    QuotaSummary,
    Subscription,
    BasicLaunch,
    CampaignV3,
    CampaignList,
    Chat,
    AsyncChunk,
}

impl ReadEndpoint {
    pub fn path(self) -> &'static str {
        match self {
            Self::Identity => "/passport/account/info/v2/",
            Self::UserLaunch => "/alice/user/launch",
            Self::Catalog => "/alice/slot/action_bar_v3/brief_list",
            Self::QuotaSummary => "/alice/commerce/sale/subscription/quota/summary/",
            Self::Subscription => "/alice/commerce/sale/subscription/overview/",
            Self::BasicLaunch => "/alice/basic/launch",
            Self::CampaignV3 => "/alice/ug/campaign/v3/detail/",
            Self::CampaignList => "/alice/ug/campaign/list/",
            Self::Chat => "/chat/completion",
            Self::AsyncChunk => "/chat/async/chunk_stream",
        }
    }

    pub fn method(self) -> &'static str {
        if matches!(self, Self::Identity) {
            "GET"
        } else {
            "POST"
        }
    }

    pub fn control(self) -> bool {
        !matches!(self, Self::Identity | Self::Catalog | Self::Chat | Self::AsyncChunk)
    }

    pub fn ug(self) -> bool {
        matches!(self, Self::CampaignV3 | Self::CampaignList)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Identity => "账号身份核对",
            Self::UserLaunch | Self::BasicLaunch => "启动配置查询",
            Self::Catalog => "模型目录查询",
            Self::QuotaSummary => "额度汇总查询",
            Self::Subscription => "订阅信息查询",
            Self::CampaignV3 | Self::CampaignList => "活动状态查询",
            Self::Chat | Self::AsyncChunk => "对话",
        }
    }
}
