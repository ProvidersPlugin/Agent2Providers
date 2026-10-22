//! WorkBuddy 的**地区**（国内版 / 国际版）：两套站点与两条 provider 身份。
//!
//! ── 这一家是什么 ────────────────────────────────────────────
//! WorkBuddy 是腾讯的编码代理客户端，同一个产品有两个站点：
//!
//! ```text
//!                    登录 / 计费 / 目录 / 推理
//!   国内版   https://copilot.tencent.com   （product WorkBuddy，platform workbuddy）
//!   国际版   https://www.workbuddy.ai      （product WorkBuddy AI，platform workbuddy-ai）
//! ```
//!
//! 协议**完全一致**（`endpoints` 模块头那句「两版协议一致、仅端点与客户端身份
//! 不同」），差别只有域名、UA 里的产品名/版本、以及 `auth/state?platform=`
//! 的取值。
//!
//! ── 为什么两个地区是两家 provider（本次拆分的由来）────────────
//! 与 Cline 的两个额度池、AutoClaw / Accio / ZCode 的两个地区同一思路 ——
//! 那三次的结论在本家同样成立，而且本家是**唯一还没拆的那个**：
//! 早先的实现把地区做成「账号上的 `edition` 字段」，后果是三处具体故障：
//!
//!   1. **目录只有一份**：`core::models` 的全局目录 + `catalog_cache` 的单槽 +
//!      `modelRefresh:workbuddy` 单条排期，于是「用国内账号拉一次、再用国际
//!      账号拉一次」会互相覆盖，两个地区的清单不可能同时存在（issue #74）；
//!   2. **模型命名空间只有一套**：启用开关 / 别名 / 能力覆盖 / 思考等级绑定
//!      全部按 `(provider, id)` 记账，同名模型在两个地区只能有一条记录，
//!      既无法区分、也无法分别点名（issue #89）；
//!   3. **转发选路不含地区**：候选账号是「workbuddy 组的全部账号」，按全局
//!      优先级挑 —— 请求完全可能落到另一个地区的账号上，而那里的上游若回
//!      400「模型不存在」，分类是 `Fatal`、不会换账号，请求直接失败。
//!
//! 按两家建模之后，目录、缓存槽、刷新排期、模型规则、别名、账号队列、报表
//! 与 Key 白名单全都按 provider id 自动分开 —— 这正是「地区不要做成账号的
//! 字段」这条约定在本家的最后一块拼图。
//!
//! ── provider id 与落盘契约 ──────────────────────────────────
//! 国内版**必须**保持 `"workbuddy"`：它是存量账号 `accounts.json` 里的落盘
//! 契约，改名会让那些账号升级后变成「未知 provider」而静默消失（与
//! `autoclaw` 不改名的理由相同）。国际版用新 id `"workbuddy-intl"`，
//! 存量国际版账号由 `account_store::migrate_startup` 原地归位。
//!
//! ── 账号 id 为什么不加地区前缀（与 AutoClaw 的差别）──────────
//! `autoclaw_accounts` 给国际版账号的 id 加了 `intl-` 前缀，理由是
//! 「同一个人的两个地区账号 userId 可能相同（同一手机号在两套系统里各自注册），
//! 撞了就会让两地账号无法并存」。本家**不存在这个风险**：WorkBuddy 的 uid 是
//! 上游生成的 uuid（实测形如 `5877cabc-99f7-4838-95f3-4834b0366838`），
//! 两套系统各自生成、撞上等于 uuid 碰撞。反过来，给国际版加前缀要**改存量
//! 账号的 id**，而 id 是 `requests.account_id` / `request_daily` 的引用键 ——
//! 改名会让用户在请求日志与报表里再也对不上自己那个账号。
//! 因此本家两地都用裸 `user-<uid>`，只有 `provider` 字段区分归属；
//! 万一真撞上，存储层的跨 provider 撞 id 保护会明确报错（不会写坏数据）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use crate::workbuddy_shared::endpoints::{resolve_edition, EditionInfo};

/// WorkBuddy 的地区。
///
/// 顺序 = 注册表顺序（国内版在前）：`ALL` 的遍历顺序决定模型目录合并时
/// 同名模型先归谁家、以及界面上两家的先后。国内版在前是因为它是存量账号的
/// 归属（与 AutoClaw 的排序理由一致）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Region {
    /// 国内版（`copilot.tencent.com`；provider id 是 `workbuddy`）
    #[default]
    Cn,
    /// 国际版（`www.workbuddy.ai`；provider id 是 `workbuddy-intl`）
    Intl,
}

impl Region {
    /// 两个地区（注册表顺序：国内版在前）
    pub const ALL: [Region; 2] = [Region::Cn, Region::Intl];

    /// 本地区的 provider id（`"workbuddy"` / `"workbuddy-intl"`）。
    ///
    /// 插件侧不再有 `ProviderKind` 那个封闭枚举（它只活在网关里）——
    /// 地区 → 身份的唯一映射就是这两个常量。
    pub fn provider_id(self) -> &'static str {
        match self {
            Self::Cn => "workbuddy",
            Self::Intl => "workbuddy-intl",
        }
    }

    /// provider id → 地区（`workbuddy` 系之外的 id 返回 None）。
    ///
    /// ── 为什么插件里没有调用方 ──────────────────────────────────
    /// 插件侧的 provider id 来自**实例自己**（`WorkBuddyProvider::region`），
    /// 不需要从账号记录反查地区 —— 那个反查是网关做的事（一个 provider 一个
    /// 实例，账号归属由网关的队列决定）。保留它是为了让「id ↔ 地区」的映射
    /// 只有一处定义，搬新代码时直接可用。
    #[allow(dead_code)]
    pub fn from_provider_id(provider_id: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|region| region.provider_id() == provider_id)
    }

    /// 展示名（界面上跟在 `WorkBuddy` 后面的那一段）
    pub fn label(self) -> &'static str {
        match self {
            Self::Cn => "国内版",
            Self::Intl => "国际版",
        }
    }

    /// 本地区的版本配置（端点 / 前缀 / platform / UA 段 / 客户端版本）。
    ///
    /// 复用 `endpoints` 的 `EditionInfo` 而不是在这里重抄一份：登录、鉴权、
    /// 目录、转发四处的端点事实来源本来就是它，重抄一份的下场是「改端点时
    /// 只改一处」的老问题（见 `endpoints` 模块头的端点表说明）。
    ///
    /// 内部走 `resolve_edition`（而不是直接引用那两个 `const`）：
    /// 那两个常量是私有的，而地区 id 与版本 id 本来就是同一套字符串
    /// （`cn` / `intl`），让别名解析只发生在那一个函数里。
    pub fn edition(self) -> &'static EditionInfo {
        resolve_edition(Some(self.id()))
    }

    /// 本地区的版本 id（`"cn"` / `"intl"`；与 `resolve_edition` 的取值同域）
    pub fn id(self) -> &'static str {
        match self {
            Self::Cn => "cn",
            Self::Intl => "intl",
        }
    }

    /// 本地区的默认端点（账号记录没写 `endpoint` 时的兜底）
    pub fn default_endpoint(self) -> &'static str {
        self.edition().endpoint
    }

    /// 本地区的版本 id（`"cn"` / `"intl"`）—— 与 [`Self::id`] 同义，
    /// 名字更直白的那一个（`session_of_record` 用它写 `edition` 字段）。
    pub fn edition_id(self) -> &'static str {
        self.id()
    }

    /// 把版本 id 归一成地区（接受 `cn` / `intl` 及常见别名，未知值回落国内版）
    /// —— 别名表在 `resolve_edition` 里，别处不要另写一份。
    ///
    /// 判据取归一后的 **id 字符串**而不是「返回的引用是不是同一个」：
    /// `resolve_edition` 返回的 `&'static` 来自 `const` 提升，两次调用是否
    /// 落在同一个地址**不是**语言保证（指针比较会时对时错）。
    pub fn from_edition_id(value: Option<&str>) -> Self {
        let id = resolve_edition(value).id;
        Self::ALL
            .into_iter()
            .find(|region| region.id() == id)
            .unwrap_or(Self::Cn)
    }

    /// 本地区默认登录态的 access token（脚本 / CI 用户的旁路入口，见
    /// `ProviderAdapter::allows_anonymous_default_session`）。
    ///
    /// ── 变量名为什么要分开 ──────────────────────────────────────
    /// 一个 token 只属于一个站点：拿国内版的 token 去打国际版端点必然 401。
    /// 拆家前 `WORKBUDDY_TOKEN` 是「本进程唯一上游」的凭证，端点由
    /// `WORKBUDDY_EDITION` 决定；现在两个地区同时存在，共用一个变量名会让
    /// 「只想给国际版配 token」变成「两地一起改」（与 ZCode 的 `env_prefix`
    /// 同一条理由）。
    ///
    /// ── 旧组合仍然可用（有意保留）───────────────────────────────
    /// `WORKBUDDY_EDITION=intl` + `WORKBUDDY_TOKEN` 是拆家前把整个进程指向
    /// 国际版的既有用法（headless / CI）。国际版因此还接受这一对旧变量 ——
    /// 只在**版本明确是 intl** 时生效，不构成「国内版的 token 被国际版误用」
    /// （那会让一个只配了国内版的机器把请求打到国际站）。
    pub fn env_token(self) -> Option<String> {
        match self {
            Self::Cn => env_text("WORKBUDDY_TOKEN"),
            Self::Intl => env_text("WORKBUDDY_INTL_TOKEN").or_else(|| {
                if env_edition_is_intl() {
                    env_text("WORKBUDDY_TOKEN")
                } else {
                    None
                }
            }),
        }
    }

    /// 本地区的端点覆盖（staging、自建网关等场景）。
    ///
    /// 国际版同样保留一对旧变量（`WORKBUDDY_ENDPOINT` + `WORKBUDDY_EDITION=intl`）
    /// 的兼容读，理由与 [`Self::env_token`] 相同。
    pub fn env_endpoint_override(self) -> Option<String> {
        match self {
            Self::Cn => env_text("WORKBUDDY_ENDPOINT"),
            Self::Intl => env_text("WORKBUDDY_INTL_ENDPOINT").or_else(|| {
                if env_edition_is_intl() {
                    env_text("WORKBUDDY_ENDPOINT")
                } else {
                    None
                }
            }),
        }
    }
}

/// 环境变量取非空字符串（空白串视为未设置）
fn env_text(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// `WORKBUDDY_EDITION` 是否把本进程指向国际版（旧组合的判据，见 `env_token`）
fn env_edition_is_intl() -> bool {
    Region::from_edition_id(env_text("WORKBUDDY_EDITION").as_deref()) == Region::Intl
}
