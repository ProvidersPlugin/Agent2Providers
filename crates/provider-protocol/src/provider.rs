//! 身份与包元数据：provider id、能力位、包清单。
//!
//! ── 为什么 `ProviderId` 是字符串而不是枚举 ────────────────────
//! 网关今天的 `ProviderKind` 是 29 个变体的封闭枚举，被引用 370 次 ——
//! 那是「provider 编译期内置」这个前提的产物。插件架构下，装了什么包是
//! **运行时**才知道的，枚举必须消失。
//!
//! 代价要说清楚：失去穷举 match 的编译期保护。补偿手段是
//! `adapter_for` 返回 `Option`（今天返回 `&'static dyn`，永不失败），
//! 调用方必须处理「没装这个 provider」—— 而这恰好也是插件架构要表达的
//! 真实状态：用户可以只装一部分包。
//!
//! `ProviderId` 是 `String` 的 newtype 而不是裸 `String`：它要落盘
//! （`accounts.data.provider`）、要出现在 HTTP 响应里、要被前端当筛选条件，
//! 用 newtype 至少能防「拿一个 label 当 id 传」这类错误。

use serde::{Deserialize, Serialize};

/// 一个 provider 的标识（落盘契约，存量 id 不能改名）。
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(pub String);

impl ProviderId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ProviderId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<String> for ProviderId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// 一个 provider 的静态能力位（握手时一次性申报，网关缓存）。
///
/// ── 为什么走握手而不是每请求 RPC ────────────────────────────
/// 这些值是**常量**：转发循环每处理一个请求都要问它们（`sse_model_rewrite`
/// 在每一帧上都要判），走 RPC 会把它们变成热路径上的同步往返。握手时
/// 申报一次、网关缓存，热路径零开销。
///
/// ── 每个位的消费方（填错会怎样）─────────────────────────────
/// 与今天 `ProviderAdapter` 的对应方法逐条对齐，见各字段的文档。判不出来
/// 时一律填 `false` —— 宁可漏掉一个优化，也不要凭猜测改字节。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Capabilities {
    /// 有推理转发能力（今天各家都是 true）。
    ///
    /// 消费方：账号选路的队首判定。填错的后果：无转发能力的家排进队首，
    /// 顶栏显示成「当前登录态」。
    #[serde(default = "yes")]
    pub chat: bool,
    /// 上游是不是**多步会话协议**（建会话 → 工具循环 → 完成）。
    ///
    /// 消费方：网关按它分流到 `conversation` 而不是 `chat`。填错的后果：
    /// 有状态家被当无状态，请求发出去但语义全错。
    #[serde(default)]
    pub stateful: bool,
    /// 能主动续期凭证。
    ///
    /// 消费方：后台凭证维护的入场券。填错的后果：声称支持但刷不动 →
    /// 每轮白跑并灌失败日志。
    #[serde(default)]
    pub refresh: bool,
    /// 有远程模型目录可拉。
    ///
    /// 消费方：目录刷新排期。填错的后果：声称支持却拿不到新清单，
    /// 用户以为坏了。
    #[serde(default)]
    pub model_refresh: bool,
    /// 能查余额 / 积分。
    ///
    /// 消费方：账号卡的「积分」按钮、批量查询的目标集合。填错的后果：
    /// 给了入口但必然失败。
    #[serde(default)]
    pub usage: bool,
    /// 有每日签到（或等价动作）。
    ///
    /// 消费方：签到中心的清单、定时任务。填错的后果：一个点了必然报错的
    /// 复选框，比没有更糟。
    #[serde(default)]
    pub checkin: bool,
    /// 有可领取的活动（新手礼 / 每日任务 / 限时套餐）。
    #[serde(default)]
    pub claims: bool,
    /// 支持「拉起官方登录页、由网关换取凭证」的网页登录。
    #[serde(default)]
    pub web_login: bool,
    /// 下发 SSE 帧时要把 `model` 改写成客户端请求的名字。
    ///
    /// 消费方：`upstream::sse` 的逐帧出口。填错的后果：客户端看到的
    /// 模型名与它请求的不一致（小浣熊上游会回内部名）。
    #[serde(default)]
    pub sse_model_rewrite: bool,
    /// 丢掉「整片只有换行」的 content 分片。
    ///
    /// 消费方：同上的逐帧出口（还要与部署开关相与）。填错的后果：
    /// 把模型真写的换行丢了，或让保活节拍污染正文。
    #[serde(default)]
    pub sse_strip_newline_chunks: bool,
    /// 允许「一个账号都没有时用环境变量凭证 / 默认登录态转发」。
    #[serde(default)]
    pub anonymous_default_session: bool,
    /// 未配代理时跟随系统代理。
    #[serde(default)]
    pub system_proxy_when_unset: bool,
    /// 凭证过期时间是否**不走账号维度**（目录接口无鉴权、清单是全局的）。
    ///
    /// 与 [`Self::model_refresh`] 配对：今天 Cline 覆写 `refresh_uses_account`
    /// 为 false。填错的后果：「获取模型」弹窗显示一个选了也一样的下拉。
    #[serde(default = "yes")]
    pub refresh_uses_account: bool,
    /// 这家认「默认模型」这个概念（客户端没指定 `model` 时网关可注入
    /// `config.defaultModel`）。
    ///
    /// ── 为什么必须有这一位（不是可选优化）──────────────────────
    /// 网关的口径是「默认值命中的 provider 里**有家认这个概念**才注入」，
    /// 都不认就按「未指定」处理、让上游用自己的默认模型。默认值是 false
    /// 的家一旦被误注入了别家的模型名，请求会被路由到 A 家、模型名却是 B 家的
    /// —— 那是一次必然 404 或答非所问的请求。
    ///
    /// 今天覆写为 true 的六家：Accio（两地区）、AtomCode、LobsterAI、Loomy、
    /// Raccoon、Qoder（两地区）。它们迁到插件后必须申报这一位。
    #[serde(default)]
    pub supports_default_model: bool,
    /// 有环境变量 / 默认登录态凭证（没有账号记录也能转发）。
    ///
    /// ── 与 [`Self::anonymous_default_session`] 的分工 ────────────
    /// `anonymous_default_session` 说的是「允许走那条旁路」，
    /// 这一位说的是「**此刻**本机真有那份凭证」。前者是恒定能力（填一次），
    /// 后者取决于运行环境（`%APPDATA%` 里有没有登录态、环境变量有没有设）。
    ///
    /// 消费方：`catalog::provider_available`（模型清单要不要广告这家）。
    /// 判错的方向只能是「少报」—— 多报会让 `/v1/models` 广告一家其实没有
    /// 凭证的家，客户端选中它就是稳定 401。
    ///
    /// ── 插件为什么只能申报静态值 ────────────────────────────────
    /// 插件读不到网关进程的环境变量，也读不到网关那边的桌面端登录态路径；
    /// 它能申报的是「我这家的实现里有这条旁路」。真正要「此刻有没有」的
    /// 场合由 [`crate::methods::gateway::TOKEN_ENSURE`] 的失败来回答 ——
    /// 那比一次静态申报更准。所以这一位在插件侧的实际语义是
    /// 「有旁路实现」，网关消费时按「可能可用」处理。
    #[serde(default)]
    pub env_credentials: bool,
}

fn yes() -> bool {
    true
}

/// 这一家**账号行怎么渲染**（插件在握手时申报，网关界面照单执行）。
///
/// ── 为什么这属于 provider 知识而不是网关知识 ────────────────
/// 同一张账号表要显示十几家，各家的公开视图字段名并不统一：标识有的叫
/// `uid`、有的叫 `userId`、有的只能拿 `id`；有效期有的在 `expiresAt`、有的在
/// `tokenExpiresAt`、有的在 `tokenExpireAt`；有的家根本没有地区概念，有的
/// 余额要换一套排版。这些**全是上游事实**，原先写成网关前端的一张
/// `PROVIDER_FEATURES` 常量表 —— 加一家插件就得回头改网关，正是插件架构要
/// 消灭的耦合。搬家后由插件自己申报，界面只按字段渲染。
///
/// ── 默认值为什么是「全关」──────────────────────────────────
/// 与 [`Capabilities`] 同一纪律：默认值不是结论，是「尚未接入」。判不出来
/// 就不要开能力位 —— 开了却做不到，用户看到的是一个点了必然失败的入口。
///
/// ── 为什么用结构体而不是一堆并列字段 ────────────────────────
/// 它们总是一起被消费（渲染一行账号），而且整体缺省（旧插件不申报 =
/// `None` = 界面走兜底）。并列的 11 个 `Option` 字段会让 `ProviderInfo`
/// 长出 11 行，也会让「这一家申报了吗」变成一个需要逐个判的问题。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountFeatures {
    /// 有余额 / 积分查询概念。
    ///
    /// 消费方：账号行的余额列与「积分」按钮、批量查询的目标集合。
    /// 与 [`Capabilities::usage`] 的区别：那个是「后端实现了查询」，这个是
    /// 「界面该给它一个入口」。绝大多数家两者一致，Command Code / MonkeyCode
    /// 是刻意两者都关（上游没有余额接口）。
    #[serde(default)]
    pub usage: bool,
    /// 有余额格的「积分明细」入口（逐份积分包：面额 / 已用 / 剩余 / 领取 → 到期）。
    ///
    /// 这是一条**承诺**：申报了就表示后端的 `query_usage` 会产出 `credits[]`，
    /// 点开一定有逐份读数。没把握的家不要开 —— 点开一个空框比没有入口更糟。
    #[serde(default)]
    pub credit_detail: bool,
    /// `credits[]` 里带**逐份到期时刻**（`expireAt`），可以按「快到期优先」排序。
    ///
    /// ── 为什么与 `credit_detail` 分开 ───────────────────────────
    /// 有明细 ≠ 明细里有到期时间。真实分布是分裂的：WorkBuddy / Trae /
    /// OfficeAce 的逐份读数带 `expireAt`；小浣熊只给余额一个数、ZCode 的纯比例
    /// 窗口连总量都没有、Qoder 的桶级口径没有时刻、Loomy / ModelScope 的两行
    /// 都是常驻额度。排序是用户**预先设定**的偏好（设置弹窗里得先能勾上），
    /// 所以它要的是静态声明，而不是「此刻读到没有」。
    ///
    /// 申报这一位的家必须真的写 `expireAt`（毫秒）；写了却读不到，那一组会
    /// 显示「暂无数据」而不是假装可用。
    #[serde(default)]
    pub credit_expiry: bool,
    /// 余额列走「套餐徽标 + 点击明细弹层」的两行形态（今天只有 CodeArts）。
    ///
    /// 与 `credit_detail` 的差别是**动不动排版**：这个会替掉读数本来的排版。
    /// 只在「一行读数放不下、必须靠弹层才分得出权益差别」的家才开。
    #[serde(default)]
    pub usage_detail: bool,
    /// 有国内 / 国际版概念（决定徽章是否拼版本后缀）。
    ///
    /// 注意与 provider id 的关系：`zcode` / `zcode-intl` 是两个 provider，
    /// 但两家都要开这一位 —— 公开形态里带着 `edition` 字段供徽章显示。
    /// 反过来，注册名自带地区的家（`WorkBuddy 国际版`）要关掉，否则会拼出
    /// 「WorkBuddy 国际版 国际版」。
    #[serde(default)]
    pub edition: bool,
    /// 账号标识落在公开视图的哪个键（`uid` / `userId` / `account` / `id`）。
    ///
    /// 按顺序取第一个有值的键。空 = 这一家没有可显示的标识（界面整项跳过）。
    /// 默认 `["userId"]` 是历史兜底：取不到时明细行自动少一项，不会显示错值。
    #[serde(default = "default_identifier")]
    pub identifier: Vec<String>,
    /// 有效期落在公开视图的哪个键（`expiresAt` / `tokenExpiresAt` /
    /// `tokenExpireAt`）。空 = 这一列没有可读的字段。
    ///
    /// ── 为什么是候选列表而不是一个键 ────────────────────────────
    /// 同一条账号可能有两种来源，字段名不同：**插件新落的**账号，公开视图
    /// 由插件的 `account_view` 产出；**升级上来的存量**账号没有插件凭证，
    /// 公开视图仍由网关的旧投影产出，而那套投影用的键名是历史契约（小浣熊
    /// 的 `tokenExpiresAt` 就是典型）。两家都要能显示，所以按序取第一个
    /// 有值的键 —— 插件申报时把自己那份写在前面，旧投影那份垫后。
    ///
    /// ⚠️ 各家的键名真的不同（OrcaTerm 就是 `tokenExpireAt`，少一个 s），
    /// 写错不会报错，只会让有效期列一直显示「—」。
    #[serde(default)]
    pub expiry: Vec<String>,
    /// 主凭证**长期有效**（没有固定到期时刻），界面显示「长期有效」。
    ///
    /// 与 `expiry` 为空串的区别是**语义**：空串只说「这一列没有可读的字段」，
    /// 读起来像「记录里缺数据」；这一位是明确断言「凭证不会到期」。
    ///
    /// 只认上游事实，两类可以申报：① 主凭证不过期、短期令牌由插件自动续期
    /// （Antigravity 的 Google refresh token、智谱清言的 refresh token）；
    /// ② 令牌本身没有有效期概念（ModelScope 的 `ms-` 令牌）。
    /// **别把「无续期、只能重登」的家放进来**（Loomy 14 天 / MonkeyCode 30 天
    /// / CatPaw 会话 Cookie）：它们的凭证有固定寿命，只是过期后救不回来。
    #[serde(default)]
    pub long_lived: bool,
    /// 有「领取体验套餐」这个动作（今天只有 ZCode 两家）。
    #[serde(default)]
    pub claim: bool,
    /// 有「使用哪个套餐（上游通道）」这个设置（今天只有 ZCode 两家）。
    ///
    /// 它回答「这个账号的请求走哪条上游通道」：编码套餐（自己买的订阅）还是
    /// 活动套餐（官方限时发放的额度）。两者是**两份独立额度**，「套餐已到期」
    /// 这类拒绝只由其中一条给出，所以它是设置项而不是自动探测。
    #[serde(default)]
    pub plan_channel: bool,
    /// 账号以邮箱报名字（Qoder / AutoClaw 国际版 / Accio）。
    ///
    /// 两处消费：账号主名的口径（未设备注时用邮箱）、拉取模型的账号下拉。
    #[serde(default)]
    pub email_as_name: bool,
    /// 并发上限的默认值（`> 0` = 本家**没有**「不限」这一档）。
    ///
    /// CodeArts 的 3 是上游硬顶（超过直接回 HTTP 400，且是账号级冲突、
    /// 不降级换号），所以那一家把 `0` 解释成「按默认 3」而不是「不做并发过滤」。
    #[serde(default)]
    pub concurrency_default: Option<u32>,
    /// 记录上没有 `limiters` 键时的缺省限制规则。
    ///
    /// ── 为什么这条也属于 provider 知识 ──────────────────────────
    /// 缺省规则是「这一家的额度形态长什么样」的结论：CodeArts 的余额读数
    /// 常判不出（免费版账号的统计里没有计量表），所以它的缺省规则是 Token
    /// 自然日限额；Cline 免费池余额贴 0 是常态，所以不给规则；其余家是
    /// 「余额 < 1 跳过」。网关原先按 provider id 写死这三条，搬家后由插件申报。
    ///
    /// ── 为什么是 `Option` 而不是空数组 ─────────────────────────
    /// 两种「没有规则」必须分得开：`None` = 这一家没申报（网关走自己的兜底），
    /// `Some(vec![])` = **明确**没有缺省规则（Cline 免费池就是这一档）。
    /// 混成一个空数组，网关就分不清「不知道」和「知道是零」。
    ///
    /// 网关**只**在账号记录上既没有 `limiters` 也没有 `lowBalance` 时用它；
    /// 用户显式配过的规则永远优先。
    #[serde(default)]
    pub default_limiters: Option<Vec<DefaultLimiter>>,
}

/// 一条缺省限制规则（[`AccountFeatures::default_limiters`] 的元素）。
///
/// ── 为什么不直接复用网关的 `LimiterRule` ────────────────────
/// 那是网关内部类型（带 `LimiterKind` / `LimiterAction` 两个私有枚举与
/// 序列化口径）。协议要表达的是同一件事，但**不能**让插件依赖网关的内部
/// 形状 —— 那等于把网关的重构成本转嫁给每个插件作者。这里用最少的字段
/// 表达三种真实形态，网关在读到时映射成自己的类型。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DefaultLimiter {
    /// 判据种类。
    pub kind: DefaultLimiterKind,
    /// 触发阈值（余额与余额列同一数字口径；Token 为原始个数）。
    pub threshold: f64,
    /// 仅 [`DefaultLimiterKind::Token`]：窗口秒数（`0` = 自然日）。
    #[serde(default)]
    pub period_seconds: u32,
    /// 命中后的动作。
    #[serde(default)]
    pub action: DefaultLimiterAction,
}

/// 缺省限制规则的判据种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultLimiterKind {
    /// 余额读数低于阈值（严格小于）。
    Balance,
    /// 重置窗口内累计 Token 消耗达到阈值（大于等于）。
    Token,
}

/// 缺省限制规则命中后的动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultLimiterAction {
    /// 跳过（余额回升 / 窗口重置后自动恢复）。
    Skip,
    /// 禁用（不自动恢复，需手动启用）。
    Disable,
}

impl Default for DefaultLimiterAction {
    fn default() -> Self {
        Self::Skip
    }
}

fn default_identifier() -> Vec<String> {
    vec!["userId".to_string()]
}

impl Default for AccountFeatures {
    fn default() -> Self {
        Self {
            usage: false,
            credit_detail: false,
            credit_expiry: false,
            usage_detail: false,
            edition: false,
            identifier: default_identifier(),
            expiry: Vec::new(),
            long_lived: false,
            claim: false,
            plan_channel: false,
            email_as_name: false,
            concurrency_default: None,
            default_limiters: None,
        }
    }
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            chat: true,
            stateful: false,
            refresh: false,
            model_refresh: false,
            usage: false,
            checkin: false,
            claims: false,
            web_login: false,
            sse_model_rewrite: false,
            sse_strip_newline_chunks: false,
            anonymous_default_session: false,
            system_proxy_when_unset: false,
            refresh_uses_account: true,
            supports_default_model: false,
            env_credentials: false,
        }
    }
}

/// 一个 provider 在界面上拥有的**管理面板**。
///
/// ── 为什么需要这个枚举（而不是让界面按 provider id 猜）─────────
/// 管理面板（伪装头编辑器、令牌池、任务树、定时排程……）的实现都在插件里，
/// 但渲染它们的是网关的界面。界面原先按 provider id 写死「这一家有哪些
/// 面板」，后果是：插件没装时面板照样显示，点进去是一屏「读取失败」。
///
/// 插件在握手时申报自己实际拥有的面板，界面按申报渲染 —— 没装插件就没有
/// 这些面板，装了才出现。加一家插件也不必改网关的界面代码。
///
/// ── 为什么用封闭枚举而不是任意字符串 ────────────────────────
/// 每个变体对应网关里一段**具体的渲染代码**。任意字符串会让插件申报一个
/// 网关不认识的键，界面只能忽略 —— 那和没申报没有区别，却让「拼写错误」
/// 变成静默失效。封闭枚举在编译期就挡住了。
///
/// ── 新增变体是兼容改动 ──────────────────────────────────────
/// 旧网关收到不认识的变体：反序列化失败会让整个 `ProviderInfo` 解析不出来，
/// 握手直接失败。所以消费侧用 `#[serde(other)]` 兜底成
/// [`AdminPanel::Unknown`]，把「不认识的面板」降级成「不显示」。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminPanel {
    /// 上游请求头的逐键覆盖编辑器（今天只有 Cline 用）。
    UpstreamHeaders,
    /// 人机验证令牌池的库存与补货状态（今天只有 ZCode 用）。
    CaptchaPool,
    /// 成长任务树：任务清单、开学季、猫猫旅行（今天只有 WorkBuddy 国内版用）。
    GrowthTasks,
    /// 每日活跃保活：模型链与定时排程（今天只有 WorkBuddy 国际版用）。
    DailyKeepalive,
    /// 网关不认识的变体（新插件配旧网关）。消费侧一律当作「不显示」。
    #[serde(other)]
    Unknown,
}

/// 一个 provider 的完整描述（`provider.info` 与握手的返回）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderInfo {
    /// provider id（落盘契约）。
    pub id: ProviderId,
    /// 界面显示名（随语言无关，前端做 i18n 时按 id 查）。
    pub label: String,
    /// 注册表顺序（前端列表排序用；同值按 id 排）。
    #[serde(default)]
    pub order: i32,
    /// 静态能力位。
    #[serde(default)]
    pub capabilities: Capabilities,
    /// 支持的登录方式（界面据此渲染「添加账号」表单）。
    #[serde(default)]
    pub login_methods: Vec<crate::account::LoginMethod>,
    /// 内置的默认模型清单（没拉过远程目录时用它）。
    ///
    /// ── 为什么由插件给而不是网关内置 ────────────────────────────
    /// 这是「上游还没连上时界面显示什么」的兜底，属于 provider 知识。
    /// 今天散在各家 `models.rs` 里，搬家后自然跟着走。
    #[serde(default)]
    pub default_models: Vec<crate::model::ModelRecord>,
    /// 这个 provider 默认要不要出网代理（多数家跟账号级代理走）。
    #[serde(default)]
    pub needs_proxy: bool,
    /// `finish_reason` 的「内容策略拒答」取值集合（握手时一次性取回）。
    ///
    /// ── 为什么放在握手而不是每次问 ──────────────────────────────
    /// 网关在**每一条流式响应**上都要判它（`upstream::refusal` 的门控），
    /// 走 RPC 会把流式路径变成「每帧一次往返」。这是常量，握手取回即可。
    ///
    /// 语义见网关 `ProviderAdapter::stream_refusal_finishes`：命中的处置是
    /// **不罚账号、换家**（内容问题，换谁都会拒）。
    #[serde(default)]
    pub refusal_finishes: Vec<String>,
    /// 正文里的「本账号配额耗尽」标记（握手时一次性取回）。
    ///
    /// 与 [`Self::refusal_finishes`] 是同一道门控的两个判据，但处置**不同**：
    /// 这条命中要**罚账号**（冷却到正文里的窗口重置时刻）。
    #[serde(default)]
    pub refusal_markers: Vec<String>,
    /// 这一家在界面上拥有的管理面板（握手时申报，空 = 没有）。
    ///
    /// ── 界面为什么需要它 ────────────────────────────────────────
    /// 面板的**实现**在插件里（走 `admin.action`），但**渲染**在网关界面。
    /// 界面原先按 provider id 写死，导致插件没装时也显示面板。改为按这里
    /// 申报的清单渲染：没装插件 → 没有 `ProviderInfo` → 没有面板。
    ///
    /// 重复项由消费侧去重（插件写重了不该让界面画两遍）。
    #[serde(default)]
    pub admin_panels: Vec<AdminPanel>,
    /// 这一家的账号行怎么渲染（握手时申报，`None` = 旧插件没申报，界面走兜底）。
    ///
    /// ── 为什么整块可缺省 ────────────────────────────────────────
    /// 老版本插件不会申报它，而网关必须继续能装上 —— 缺省时界面按 id 查一张
    /// 兜底表（过渡期保留），所以旧插件不会因为少这一段而显示错。
    #[serde(default)]
    pub account_features: Option<AccountFeatures>,
    /// 这一家的签到能力申报（`None` = 旧插件没申报，网关走内置兜底）。
    ///
    /// ── 为什么签到要单独一块 ────────────────────────────────────
    /// 签到是**反代提供商独有的功能**：自定义 API 提供商（`custom-*`）结构上
    /// 不可能有它 —— `supports_checkin` 对它们恒为 false，签到执行体也只在
    /// 插件里。所以「这家能不能签、链路长什么样、有没有新手福利」全是 provider
    /// 知识，不该由网关查表。
    #[serde(default)]
    pub checkin: Option<CheckinInfo>,
}

/// 一个 provider 的**签到能力申报**。
///
/// ── 这个结构解决什么问题 ────────────────────────────────────
/// 网关的签到中心原先靠一张 `CHECKIN_PROVIDERS: [&str; 17]` 常量表 +
/// `supports_checkin` 里的一串特判（`edition == "intl"` / `isGuest` /
/// Accio 家族）来决定「谁能签」。这些都是上游事实，加一家就得改网关。
/// 改成插件申报之后，网关只按字段渲染，签到中心的分组、说明、按钮全部自动。
///
/// ── 与 `Capabilities::checkin` 的关系 ───────────────────────
/// 那一位是「后端实现了签到」，这一块是「界面怎么呈现它」。绝大多数家两者
/// 一致；分成两处是因为有的家能签但界面要额外说明（见 `description`），
/// 有的家能签但要按账号形态排除（见 `exclude_guest`）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CheckinInfo {
    /// 这一家的签到链路一句话（签到中心分组行下面的说明）。
    ///
    /// 由插件提供而不是界面查表：这句话描述的是**上游怎么发积分**
    /// （「腾讯每日签到接口」/「桌面端每日积分链路」/「调用即触发当日魔粒」），
    /// 是纯 provider 知识，而且会随上游变化。
    #[serde(default)]
    pub description: Option<String>,
    /// 访客账号不参与签到（智谱清言那一家）。
    ///
    /// ── 为什么按账号排除而不是按家 ──────────────────────────────
    /// 访客没有积分体系（相关接口一律 401），列进签到范围只会永远显示「待签」
    /// 并把进度分母撑大。判据读公开形态的 `isGuest` —— 那是插件自己写进
    /// `account_view` 的，网关不解析凭证。
    #[serde(default)]
    pub exclude_guest: bool,
    /// 这条签到链路支持**分档执行**（`checkin` / `keepalive` / `claim` / `full`）。
    ///
    /// WorkBuddy 国际版的「每日活跃」是三件独立的事（保活点亮 / 领取加积分 /
    /// 两者都做），所以界面上要给三颗按钮而不是一颗。别的家只有一颗。
    #[serde(default)]
    pub modes: Vec<CheckinMode>,
    /// 有没有「新手福利」（一次性任务，签到后自动查询并领取）。
    #[serde(default)]
    pub onboarding: bool,
    /// 有没有「活动套餐领取」（ZCode 那种限时套餐）。
    #[serde(default)]
    pub plans: bool,
    /// 是否参与**每日签到**（默认 `true`）。
    ///
    /// ── 为什么需要这一位 ────────────────────────────────────────
    /// 在这之前，「申报了 `CheckinInfo`」就等同于「有每日签到」
    /// （见网关 `billing::checkin::supports_checkin`）。但 ZCode 是反例：
    /// 它没有每日签到（`Capabilities::checkin` 恒 false），只有「活动福利」
    /// ——限时体验套餐领取。为了让它出现在活动福利那张卡里，它必须申报
    /// `plans: true`；而一旦申报，就会被当成有签到链路拉进每日签到，
    /// 结果是每天落一条必然失败的记录、把签到进度分母撑大。
    ///
    /// 所以把两件事拆开：`daily` 管每日签到，`plans` / `onboarding` /
    /// `growth` 各管自己那张卡。默认 `true` 是为了让**既有插件**的行为
    /// 一字不变 —— 它们都真的有每日签到。
    #[serde(default = "yes")]
    pub daily: bool,
    /// 有没有「成长任务」（WorkBuddy 国内版的任务树）。
    #[serde(default)]
    pub growth: bool,
    /// 签到中心里这一家专属的**附加 UI 钩子**（空 = 没有）。
    ///
    /// ── 为什么是枚举而不是自由字符串 ────────────────────────────
    /// 每个变体对应客户端里一段具体渲染（Qoder 国际版的 UMID 组件提示）。
    /// 自由字符串会让插件申报一个客户端不认识的键，界面只能忽略 ——
    /// 那和没申报一样，却让拼写错误变成静默失效。
    #[serde(default)]
    pub ui_hooks: Vec<CheckinUiHook>,
}

/// 签到执行的档位（界面据此决定给几颗按钮）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckinMode {
    /// 标准签到（一次动作完成）。
    Checkin,
    /// 只保活（维持活跃、不领奖励）。
    Keepalive,
    /// 只领取（探测活动并领取，不做保活）。
    Claim,
    /// 保活 + 领取（与定时签到同一套组合）。
    Full,
}

/// 签到中心里的附加 UI 钩子（客户端认得才渲染）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckinUiHook {
    /// Qoder 国际版的 UMID 设备风控组件提示与一键安装。
    UmidComponent,
    /// 网关不认识的钩子（新插件配旧客户端），一律当作「不显示」。
    #[serde(other)]
    Unknown,
}

/// 一个包提供的 provider（包清单里的一项，比 `ProviderInfo` 轻）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackProvider {
    pub id: ProviderId,
    pub label: String,
    #[serde(default)]
    pub order: i32,
    /// 这一项是否默认启用（用户可以单独关掉一家）。
    #[serde(default = "yes")]
    pub default_enabled: bool,
}

/// 包清单（`pack.json`）。
///
/// ── 这个文件是安装期的**唯一**信任依据 ──────────────────────
/// 网关读它决定：这个包声称提供哪些 provider、协议版本对不对、二进制在哪。
/// 它自己不能自证（谁都能写一份），所以真实信任来自签名与 sha256 ——
/// 见网关侧 `plugin_host::install` 的校验顺序。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackManifest {
    /// 包 id（例如 `agent2api-providers`）。
    pub id: String,
    /// 显示名。
    pub name: String,
    /// 包版本（语义化版本）。
    pub version: String,
    /// 这个包实现的协议版本，必须等于 [`crate::PROTOCOL_VERSION`]。
    pub protocol: u32,
    /// 包提供的 provider 列表。
    pub providers: Vec<PackProvider>,
    /// 入口二进制，按目标三元组索引（`{"x86_64-pc-windows-msvc": "bin/providers.exe"}`）。
    pub entry: std::collections::BTreeMap<String, String>,
    /// 图标目录（可选，相对包根）。
    #[serde(default)]
    pub icons: Option<String>,
    /// 包主页（可选，界面上「查看来源」用）。
    #[serde(default)]
    pub homepage: Option<String>,
    /// 这个包贡献的**界面页面**（可选，空 = 纯后端包）。
    ///
    /// ── 为什么插件能带界面 ──────────────────────────────────────
    /// 有些功能只有装了插件才有意义（签到中心：自定义 API 提供商结构上不可能
    /// 有签到），把它们留在客户端意味着「每加一个插件，每个客户端都要改一遍」。
    /// 让包自己带页面之后，客户端只负责「把清单里的页面挂到菜单上」。
    ///
    /// 页面是**静态资源**（HTML + JS），由网关在 `/plugins/{pack}/ui/` 下托管，
    /// 客户端用 iframe 嵌。之所以不是「加载进宿主的 JS 运行时」：插件页面
    /// 需要完整能力（图表、表格、表单），共享宿主框架会把 React 实例、样式、
    /// 路由全都搅在一起；iframe 天然隔离，插件想用什么框架都行。
    ///
    /// ── 安全 ────────────────────────────────────────────────────
    /// UI 资源**必须**被签名覆盖（见 [`crate::signing_payload`] 的 `ui` 项）：
    /// 否则一个包可以带合法签名的二进制 + 一个偷面板令牌的 JS。
    #[serde(default)]
    pub ui: Vec<PackUiPage>,
    /// 发布者签名（可选；缺失 = 未签名包，允许安装但界面标黄）。
    ///
    /// ── 签的是什么（改动要两边一起改）──────────────────────────
    /// 覆盖的是**内容**，不是 zip 字节 —— zip 的压缩方式与时间戳可以变，
    /// 内容才是要保护的东西。载荷由 [`crate::signing_payload`] 定义，签名工具
    /// （`agent2api-providers/scripts/sign.mjs`）与网关（`plugin_host::install`）共用那一份
    /// 拼装规则，两处写岔就是「所有包都验不过」。
    #[serde(default)]
    pub signature: Option<PackSignature>,
}

/// 包贡献的一个**界面页面**（客户端按它生成菜单项并 iframe 嵌入）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackUiPage {
    /// 页面 id（包内唯一，也是客户端记住「上次停在哪」的键）。
    ///
    /// 落盘契约：插件改了它，用户的菜单位置偏好就丢了（会退回默认位置），
    /// 所以发布之后不要改名。
    pub id: String,
    /// 菜单标题。
    pub title: String,
    /// 菜单图标键（客户端认得的图标名；不认识的画首字）。
    #[serde(default)]
    pub icon: Option<String>,
    /// 入口 HTML（相对包根，例如 `ui/checkin/index.html`）。
    pub entry: String,
    /// 菜单项默认归到哪一组（客户端的**内置分组 id**，如 `g-overview`）。
    ///
    /// ── 为什么插件要申报这个 ────────────────────────────────────
    /// 客户端把插件页补进侧边栏时得知道「它属于哪一族」：签到中心是「看一眼
    /// 现状」的概览项，模型任务树可能是「转发链路」的扩展。客户端猜不出来 ——
    /// 它不认识插件的业务。不申报时归到第一组（概览），这是最不容易放错的
    /// 位置：放错到「诊断」会让用户以为它是排障工具。
    ///
    /// 值必须是客户端认得的内置分组 id（`g-overview` / `g-forward` /
    /// `g-diag` / `g-system`）；不认识的 id 客户端按「未申报」处理，**不报错**
    /// —— 分组 id 是客户端的事实，插件说错了不该让整个页面消失。
    ///
    /// 用户拖过之后以用户存的位置为准（申报值只影响「首次出现时补到哪」）。
    #[serde(default)]
    pub group: Option<String>,
    /// 排序（同值按 id 排）。默认排在客户端内置页面之后。
    #[serde(default)]
    pub order: i32,
}

/// 包清单里的签名块。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackSignature {
    /// 签名算法。当前只认 `ed25519`（写别的值一律拒绝，不静默跳过）。
    pub algorithm: String,
    /// 密钥 id：网关据此在**内置公钥表**里找对应那把（支持轮换）。
    #[serde(rename = "keyId")]
    pub key_id: String,
    /// 签名本体（base64 的 64 字节 ed25519 签名）。
    pub value: String,
    /// 这份签名覆盖的**目标平台**（`x86_64-pc-windows-msvc` 这类）。
    ///
    /// 一个包可能带多个平台的二进制，而签名只覆盖其中一个（见
    /// [`signing_payload`] 的 `binary` 项）。网关只验与自己平台一致的那份 ——
    /// 缺失或对不上都按「没有适用于本平台的签名」处理（允许安装但标未签名）。
    #[serde(default)]
    pub target: Option<String>,
}

/// 验签载荷：**包内容**的规范化摘要，不是 zip 字节。
///
/// ── 为什么不直接签 zip ─────────────────────────────────────
/// zip 里带着时间戳、压缩算法、条目顺序，同一份内容用不同工具打出来字节不同。
/// 签 zip 意味着「只有用我们那个脚本打的包才验得过」，而签名要保护的是**内容**
/// （哪个包、哪个版本、哪几家的实现、哪个二进制）。
///
/// ── 载荷怎么拼（改动必须两边同步）───────────────────────────
/// ```text
/// agent2api-pack-v1\n
/// id=<id>\n
/// version=<version>\n
/// protocol=<protocol>\n
/// target=<target>\n
/// providers=<id>:<order>:<default_enabled>,…\n     ← 按 id 字典序
/// entry=<target>=<path>,…\n                        ← 按 target 字典序
/// binary=<sha256 of the entry binary for the signed target>\n
/// ui=<relpath>=<sha256>,…\n                         ← 按路径字典序（无 UI 时为空）
/// ```
/// 末尾不留空行。`binary` 那一项是**关键**：没有它，签名只覆盖元数据，
/// 攻击者可以把 `bin/` 里的二进制换掉而签名照样成立。
///
/// `target` 也在载荷里：一个包可能带多个平台的二进制，把平台写进被签内容，
/// 「给 Windows 包签的名」就不能被挪去顶替 Linux 包（两边的 `binary` 不同，
/// 但显式写出来比依赖这一点更清楚）。
///
/// `binary_sha256` 由调用方传入（签名工具算它自己的二进制；网关算包里那个）。
/// 多平台包每个平台的二进制不同 —— 所以签名是**按 target 分开的**：
/// 网关只验自己这个 target 的二进制，`signature.target` 记的就是它。
///
/// ── `ui` 项：为什么必须签 UI 资源 ──────────────────────────
/// 插件页面跑在管理面板的 origin 下、能拿到面板令牌（等于能调所有 protected
/// API）。只签二进制的话，一个包可以带合法签名的 exe **加**一个偷令牌的
/// `checkin.js`，验签照样通过 —— 那等于给了一条绕过签名的后门。
///
/// 拼法：把 UI 目录下**每个文件**的 `相对路径=sha256` 按路径字典序排好、
/// 逗号连接。没有 UI 资源时该项为空串（老包的行为因此完全不变）。
/// `ui_files` 由调用方传入（签名工具遍历目录算；网关遍历解出来的包算）。
///
/// ⚠️ 顺序敏感：两边都必须按**相对包根的 POSIX 路径**排序（`\` 统一成 `/`），
/// 否则 Windows 上打的包在 Linux 上验不过。
pub fn signing_payload(
    manifest: &PackManifest,
    target: &str,
    binary_sha256: &str,
    ui_files: &[(String, String)],
) -> String {
    let mut providers: Vec<String> = manifest
        .providers
        .iter()
        .map(|item| {
            format!(
                "{}:{}:{}",
                item.id.0,
                item.order,
                if item.default_enabled { 1 } else { 0 }
            )
        })
        .collect();
    providers.sort();
    let entries: Vec<String> = manifest
        .entry
        .iter()
        .map(|(target, path)| format!("{target}={path}"))
        .collect();
    let mut ui: Vec<String> = ui_files
        .iter()
        .map(|(path, hash)| format!("{path}={hash}"))
        .collect();
    ui.sort();
    format!(
        "agent2api-pack-v1\nid={}\nversion={}\nprotocol={}\ntarget={}\nproviders={}\nentry={}\nbinary={}\nui={}\n",
        manifest.id,
        manifest.version,
        manifest.protocol,
        target,
        providers.join(","),
        entries.join(","),
        binary_sha256,
        ui.join(","),
    )
}
