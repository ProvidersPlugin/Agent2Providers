//! 原生 region.rs 的 2026-10-16 实测：两站登录信息接口不接受对方令牌；
//! HavanaId 也可能重号，故身份前缀、目录和权限均按 provider 分开。

#[derive(Clone, Copy)]
pub enum Region {
    Cn,
    Intl,
}

impl Region {
    pub fn provider_id(self) -> &'static str {
        match self {
            Self::Cn => "modelscope",
            Self::Intl => "modelscope-intl",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Cn => "国内版",
            Self::Intl => "国际版",
        }
    }

    pub fn order(self) -> u32 {
        match self {
            Self::Cn => 2,
            Self::Intl => 3,
        }
    }

    pub fn inference_base(self) -> &'static str {
        match self {
            Self::Cn => "https://api-inference.modelscope.cn/v1",
            Self::Intl => "https://api-inference.modelscope.ai/v1",
        }
    }

    pub fn site_base(self) -> &'static str {
        match self {
            Self::Cn => "https://modelscope.cn",
            Self::Intl => "https://www.modelscope.ai",
        }
    }

    pub fn account_prefix(self) -> &'static str {
        match self {
            Self::Cn => "modelscope-",
            Self::Intl => "modelscope-intl-",
        }
    }
}
