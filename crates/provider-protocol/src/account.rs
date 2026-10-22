//! 账号面：凭证形态、登录流程、余额与签到。
//!
//! ── 这一层的核心取舍：`secret` 与 `public` 都是不透明 JSON ───────
//! 网关**不解析**凭证内容，只负责存、取、以及在需要时原样交给插件。
//! 于是：
//!   - 加一家 provider 不需要改网关的账号层（今天 `api/accounts.rs` 的
//!     92 处分支、`store_view::public_account` 的 22 个分支全部消失）；
//!   - 整库备份 / 恢复、云同步、导入导出零改动（它们本来就只搬 JSON）；
//!   - 代价是网关无法对凭证做任何校验 —— 这由插件在
//!     [`crate::account::NormalizedAccount`] 里给出 `public` 视图来弥补，
//!     界面显示的全部字段都来自 `public`。
//!
//! ── `secret` 里绝不能有的东西 ──────────────────────────────
//! 任何要显示给用户的内容都不该只在 `secret` 里。网关的公开视图只读
//! `public`，`secret` 在 `store_view::public_account` 那条路上被整块剥掉
//! （今天的行为），插件若把「账号名」只写进 `secret`，界面就是一片空白。

use serde::{Deserialize, Serialize};

/// 凭证本体（不透明，网关不解析）。
///
/// 约定：插件可以自由组织内部结构，但**必须**能在 `normalize` 之后重复
/// 读写而不丢字段 —— 网关的整库备份会把它原样搬到另一台机器。
pub type AccountSecret = serde_json::Value;

/// 账号的公开视图（给界面看的字段）。
///
/// ── 这里的字段是**约定**，不是强类型 ─────────────────────────
/// 网关把它原样透给前端（`/api/session` 的 `accounts[]`），前端按既有的
/// 键名读取（`name` / `uid` / `tokenTail` / `expiresAt` …）。因此插件必须
/// 沿用这些键名，否则界面显示不出来 —— 这条约定要写进 SDK 文档，不能只
/// 靠网关侧兜底。
///
/// 已知键（与今天 `store_view::public_account` 的输出一致）：
/// `name`、`uid`、`nickname`、`tokenTail`、`expiresAt`、`edition`、
/// `enterpriseName`、`plan`、`avatar`、`email`、`phone`、`longLived`。
pub type AccountView = serde_json::Value;

/// `account.normalize` 的输入来源。
///
/// ── 为什么把这四种形态统一成一个方法 ──────────────────────────
/// 今天它们是 `api/accounts.rs` 里四个不同的入口（手填 / 文件内容 /
/// 文件 base64 / 导入桌面端登录态），每家各写一遍。它们的**产出**是同一个
/// 东西（一条可落库的账号），差别只在解析来源 —— 所以用枚举表达来源，
/// 让插件在一个方法里处理，网关只负责把 payload 拆成这四种之一。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NormalizeSource {
    /// 用户在界面上手填的字段（键名由前端表单决定，逐家不同）。
    Manual {
        /// 表单提交的原始 JSON（含 `name`、令牌、地区等）。
        payload: serde_json::Value,
    },
    /// 用户选了一个文本凭证文件，前端已把内容读成字符串。
    FileText {
        /// 文件全文。
        content: String,
        /// 原文件名（只用于错误文案与日志）。
        file_name: Option<String>,
    },
    /// 用户选了一个二进制文件（KukuAI 的 Cookies SQLite 这类）。
    FileBase64 {
        /// base64 编码的文件字节。
        content: String,
        file_name: Option<String>,
    },
    /// 从本机已安装的客户端实时读取登录态。
    ///
    /// ── 与「文件导入」的关键差别 ────────────────────────────────
    /// 这条路径**不落凭证**：插件产出的 `secret` 应该是一个「指向本机
    /// 某处的引用」（今天各家叫 `desktop-auth` 这类记录），转发时再实时读。
    /// 落一份快照会让「客户端重新登录」后网关还拿着旧 token。
    Desktop,
}

/// `account.normalize` 的输入。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NormalizeInput {
    /// provider id。
    pub provider: String,
    /// 来源形态。
    pub source: NormalizeSource,
    /// 用户指定的账号备注名（`None` = 插件自己取名）。
    #[serde(default)]
    pub name: Option<String>,
    /// 该账号的出网代理（网关已解析；插件在 normalize 阶段要出网验证时用它）。
    #[serde(default)]
    pub proxy: Option<crate::proxy::WireProxy>,
}

/// `account.normalize` 的产出：一条可落库的账号。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NormalizedAccount {
    /// 凭证本体（网关原样存进 `accounts.data.secret`）。
    pub secret: AccountSecret,
    /// 公开视图（网关原样透给前端）。
    pub public: AccountView,
    /// 账号的**稳定身份**，用于去重。
    ///
    /// ── 为什么必须有它 ──────────────────────────────────────────
    /// 同一个账号用不同凭证重复添加时（换了一次令牌、从另一台机器导入），
    /// 网关要认出「这是同一个人」并更新而不是新增。今天各家在自己代码里
    /// 用 `uid` / `HavanaId` / `loginName` 做这件事，规则各不相同。
    ///
    /// 规则：**同一账号必须恒定，不同账号必须不同**。空串 = 无法判定身份
    /// （网关退化成按凭证内容去重，可能产生重复记录，界面上要提示）。
    pub identity: String,
    /// 这条记录是不是「实时读本机客户端」型（对应今天的 `desktop-auth`）。
    ///
    /// 网关据此在账号行上显示「桌面端」徽章，并且在删除时不去碰本机文件。
    #[serde(default)]
    pub desktop: bool,
}

/// 登录方式（`provider.info` 里申报）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoginMethod {
    /// 粘贴令牌 / API Key。
    Token {
        /// 界面上的字段标签。
        label: String,
        /// 输入框占位提示。
        placeholder: String,
        /// 是否需要用户额外填一个 uid / 地区之类的字段。
        #[serde(default)]
        extra_fields: Vec<LoginField>,
    },
    /// 拉起浏览器做 OAuth / 设备授权。
    Browser {
        /// 按钮文案。
        button: String,
        /// 说明文字（可以是 HTML 片段）。
        #[serde(default)]
        hint: Option<String>,
    },
    /// 手机号验证码。
    Sms {
        /// 说明文字。
        #[serde(default)]
        hint: Option<String>,
    },
    /// 从本机客户端导入（不需要用户输入任何东西）。
    Desktop {
        /// 按钮文案。
        button: String,
        /// 本机没装客户端时的提示。
        #[serde(default)]
        hint: Option<String>,
    },
    /// 访客 / 免登录（智谱清言那条路）。
    Guest,
}

/// 登录方式里的一个额外输入字段。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoginField {
    /// 请求体键名。
    pub key: String,
    /// 界面标签。
    pub label: String,
    /// 占位提示。
    #[serde(default)]
    pub placeholder: String,
    /// 是否可选。
    #[serde(default)]
    pub optional: bool,
    /// 是否是多行文本（粘贴整个 JSON 凭据时用）。
    #[serde(default)]
    pub multiline: bool,
}

/// `login.start` 的返回。
///
/// ── 两种登录形态（设备授权 vs 回调）──────────────────────────
///   - 有 `url` 且 `poll_interval_ms > 0`：设备授权。网关打开 `url`，
///     然后按间隔轮询 `login.poll`；
///   - 有 `url` 且 `poll_interval_ms == 0`：回调型。网关打开 `url`，
///     等用户完成后调用 `login.callback`（带一次性 code）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoginStart {
    /// 登录流程 id（后续 `poll` / `callback` / `cancel` 都用它）。
    pub flow: String,
    /// 用户要打开的授权地址（可能为空：某些家只需要用户在界面上填东西）。
    #[serde(default)]
    pub url: String,
    /// 给用户看的说明（例如「在弹出的窗口里完成登录」）。
    #[serde(default)]
    pub instructions: String,
    /// 轮询间隔（毫秒）；`0` = 这是回调型流程，不要轮询。
    #[serde(default)]
    pub poll_interval_ms: u64,
    /// 这条流程要不要在**系统浏览器**里打开（而不是内嵌窗口）。
    ///
    /// ── 为什么需要这个开关 ──────────────────────────────────────
    /// 有些家的登录页会拒绝在 WebView 里运行（检测 UA / 依赖第三方
    /// 密码管理器），内嵌窗口表现为白屏。今天这个信息硬编码在桌面壳的
    /// `allowed_hosts` 里，搬走后必须由插件申报。
    #[serde(default)]
    pub external_browser: bool,
    /// 登录页会导航到哪些域（网关据此做导航白名单）。
    ///
    /// ── 这条为什么必须由插件给 ──────────────────────────────────
    /// 今天桌面壳 `src/login.rs` 里有一张硬编码的 provider → 域名表，
    /// 漏一家的症状是「点登录没反应」或「窗口一片空白」，而且网页形态
    /// 不走壳、沙盒里测不出来。改成插件申报之后，加一家不必再改壳。
    ///
    /// 空列表 = 不限制（只对 `external_browser = true` 有意义）。
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
}

/// `login.poll` 的返回。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LoginPoll {
    /// 还没完成，继续轮询。
    Pending {
        /// 可选的进度提示（「已扫码，请在手机上确认」）。
        #[serde(default)]
        hint: Option<String>,
    },
    /// 完成了，这是落库的账号。
    Done {
        /// 与 `account.normalize` 的产出同形。
        account: NormalizedAccount,
    },
    /// 失败了（用户取消、上游拒绝、超时）。
    Failed { message: String },
}

/// `login.callback` 的输入。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoginCallback {
    /// 流程 id。
    pub flow: String,
    /// 回调带回的一次性 code。
    pub code: String,
    /// 回调 URL 的完整形态（有些家要从里面取额外参数）。
    #[serde(default)]
    pub url: String,
}

/// `login.cancel` 的输入。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoginCancel {
    pub flow: String,
}

/// 登录流程的标识（供 `provider.info` 之外的场合引用）。
pub type LoginFlow = String;

/// `usage.query` 的返回：归一化后的余额 / 额度读数。
///
/// ── 字段是**可选集合**，不是强类型 ─────────────────────────
/// 各家能读到的东西差别很大（腾讯的积分简报、小浣熊的钱包 + 订阅权益、
/// CatPaw 的 credit 域、CodeArts 的月度配额……）。网关只做一件事：
/// 把它原样交给界面，界面按已知键名渲染。因此这里是 `Value`。
///
/// 已知键（与今天各家 `balance.rs` 的输出一致）：`plan`、`balance`、
/// `credits`、`used`、`limit`、`until`、`windows[]`、`error`。
pub type UsageReport = serde_json::Value;

/// `checkin.claim` 的结果。
///
/// ── 六种结果各自对应什么处置（与 magpie 同款，但要按本项目的语义读）──
///   - `Claimed`：刚签到成功，今天的记录写成「已领」；
///   - `Done`：今天已经签过了，同样记为「已领」，不再重试；
///   - `Ineligible`：这个账号不参与（访客、未验证邮箱），记为「不适用」；
///   - `Inactive`：当前没有可领的活动（限时活动已结束），记为「今日无活动」；
///   - `Captcha`：上游要验证码。**永远不去解**，提示用户去官方 App 操作，
///     记为「需人工」；
///   - `Failed`：真失败，按重试策略再来（本项目是 30 分钟后重试一次）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckinOutcome {
    Claimed,
    Done,
    Ineligible,
    Inactive,
    Captcha,
    Failed,
}

/// `checkin.claim` 的完整返回。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckinReport {
    pub outcome: CheckinOutcome,
    /// 这次领到了什么（积分 / 天数 / 套餐名），给界面显示。
    #[serde(default)]
    pub credit: Option<String>,
    /// 连续签到天数（有的家有这个概念）。
    #[serde(default)]
    pub streak: Option<u32>,
    /// 给用户看的一句话（`Captcha` 时是「请到官方 App 签到」这类）。
    #[serde(default)]
    pub message: Option<String>,
}

impl CheckinReport {
    pub fn new(outcome: CheckinOutcome) -> Self {
        Self { outcome, credit: None, streak: None, message: None }
    }
}
