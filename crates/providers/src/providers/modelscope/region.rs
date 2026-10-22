//! ModelScope（魔搭）的**地区**（国内站 / 国际站）：两套域名与两条 provider 身份。
//!
//! ── 上游长什么样（2026-10-16 实测）──────────────────────────────
//! 魔搭是**两个站点各一套账号体系**，同一份协议、不同的域：
//!
//! ```text
//!                推理（OpenAI 兼容）                      网页站（身份 / 魔粒 / 签到）
//!   国内  https://api-inference.modelscope.cn/v1        https://modelscope.cn
//!   国际  https://api-inference.modelscope.ai/v1        https://www.modelscope.ai
//! ```
//!
//! ── 一处**实测确认**的关键事实：一个 `ms-` key 两用 ────────────
//! 同一个 `ms-...` 令牌既作 `Authorization: Bearer` 调推理，又作
//! `Cookie: m_session_id=<key>` 调网页站的身份 / 魔粒接口 —— 两条链路都实测 200。
//! 因此本家**只有一个凭证字段**。
//!
//! ── 另一处实测确认：**两个站点的账号体系是分开的** ──────────────
//! 用国内站的 key 打 `https://www.modelscope.ai/api/v1/users/login/info`
//! 得到 `401 {"Code":10019901001,"Message":"access forbidden"}`；反过来同理。
//! 而**推理端点两地都能用同一个 key 打通**（模型网关的 key 是账号级的）。
//! 所以「地区」在本家不是可有可无的标签：**签到与魔粒必须打对站点**。
//!
//! ── 为什么是两个 provider 而不是「一家的一个选项」─────────────────
//! 地区做成「账号上的一个字段」会让两个站的模型清单互相覆盖（单槽缓存），
//! 且账号无法按站隔离。拆成两个 provider 后各家有独立的账号、清单、启停。
//!
//! ── 为什么没有环境变量覆盖 ──────────────────────────────────
//! 这两个域名是**硬编码在官方 SDK / 客户端里的**、实测稳定，为它们加一层
//! `MODELSCOPE_*_BASE_URL` 只是多一条没人会用的分支。真要换域名时改本文件
//! 的两处常量比找环境变量更直接。

/// ModelScope 的地区。
///
/// 顺序 = 注册表顺序（国内版在前）：`ALL` 的遍历顺序决定界面上两家的先后。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Region {
    /// 国内站（`modelscope.cn`；provider id `modelscope`）
    #[default]
    Cn,
    /// 国际站（`www.modelscope.ai`；provider id `modelscope-intl`）
    Intl,
}

impl Region {
    /// 本地区的 provider id（`"modelscope"` / `"modelscope-intl"`）
    pub fn provider_id(self) -> &'static str {
        match self {
            Self::Cn => "modelscope",
            Self::Intl => "modelscope-intl",
        }
    }

    /// 展示名（界面上跟在 `ModelScope` 后面的那一段）
    pub fn label(self) -> &'static str {
        match self {
            Self::Cn => "国内版",
            Self::Intl => "国际版",
        }
    }

    /// 推理基址（**含** `api-inference` 主机与尾部的 `/v1`）。
    ///
    /// `build_chat` 在它后面拼 `/chat/completions`，`models` 在它后面拼
    /// `/models` —— 两处都不要再写域名。
    pub fn inference_base_url(self) -> &'static str {
        match self {
            Self::Cn => "https://api-inference.modelscope.cn/v1",
            Self::Intl => "https://api-inference.modelscope.ai/v1",
        }
    }

    /// 网页站基址（身份 / 魔粒余额 / 今日用量 / 签到都在这个域下，
    /// **不带尾部斜杠**）。
    pub fn site_base_url(self) -> &'static str {
        match self {
            Self::Cn => "https://modelscope.cn",
            Self::Intl => "https://www.modelscope.ai",
        }
    }

    /// 本地区账号 id 的**前缀**。
    ///
    /// 两地带前缀的理由：两站的身份 id 空间互不相通，但**同一个人在两地的
    /// `HavanaId` 完全可能相同**（各自注册，服务端 id 生成规则同形就会撞）。
    /// 撞了的话网关的跨 provider 撞 id 保护会拒绝，「两地账号并存」就变得
    /// 不可能 —— 而并存正是本功能的意义之一。
    pub fn account_id_prefix(self) -> &'static str {
        match self {
            Self::Cn => "modelscope-",
            Self::Intl => "modelscope-intl-",
        }
    }
}
