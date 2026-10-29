//! `Provider` trait：插件作者唯一要实现的东西。
//!
//! ── 为什么是 trait 而不是「注册一堆闭包」──────────────────────
//! 闭包方案在方法数量少时更轻，但这里要表达的是「一家 provider 的完整
//! 知识」—— 30 多个方法、大量带默认实现的能力位。trait 的默认实现让
//! 「只做转发、别的都不支持」的一家用十几行就能接上，而 `impl` 的完整性
//! 由编译器保证。
//!
//! ── 异步签名为什么是 `Pin<Box<dyn Future>>` ──────────────────
//! 与网关侧同一理由：trait 必须 object-safe（要放进 `Vec<Box<dyn Provider>>`），
//! 而原生 async fn in trait 对 object-safe 的支持要求 Rust 1.75+ 且会产生
//! 与 `async_trait` 类似的额外分配。手写 Box 的代价只在签名上，收益是不
//! 引入 `async-trait` 依赖 —— 插件作者的依赖树越短越好。
//!
//! ── 默认实现的口径 ─────────────────────────────────────────
//! 每个方法都给默认值，且默认值一律是「不支持」或「空」。理由与网关侧
//! 一致：**默认值不是结论**，它是「尚未接入」的语义。判不出来就返回
//! `false`，宁可漏掉一个优化也不要凭猜测打上游。

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use agent2api_provider_protocol as proto;
use serde_json::Value;

/// 装箱的 provider（`serve()` 的参数类型）。
pub type BoxProvider = Box<dyn Provider>;

/// 插件作者用来标记「本地前置条件不成立」的**错误前缀**。
///
/// ── 为什么是前缀而不是换返回类型 ────────────────────────────
/// `Provider::build_chat` 的错误通道是 `String`（那是这套 SDK 里所有回调的
/// 统一形状），换类型会牵动每一个实现。用一条**明确的**前缀把分类透出去：
/// SDK 在回帧时把它翻成 [`proto::WireError::LOCAL_PRECONDITION`]，网关据此
/// 还原成 556 并让编排层顺延。
///
/// 前缀选 `LOCAL_PRECONDITION:` 而不是「文案里含某几个字」：作者改文案不会
/// 影响分类，而漏写前缀的后果只是「这条错误按普通失败处理」—— 与今天一致。
pub const LOCAL_PRECONDITION_PREFIX: &str = "LOCAL_PRECONDITION:";

/// 把一条本地前置条件错误包装成带前缀的形态。
pub fn local_precondition(message: impl AsRef<str>) -> String {
    format!("{LOCAL_PRECONDITION_PREFIX}{}", message.as_ref())
}

/// 拆开前缀：`Some(原文)` = 这是一条本地前置条件错误。
pub fn split_local_precondition(message: &str) -> Option<&str> {
    message.strip_prefix(LOCAL_PRECONDITION_PREFIX)
}

/// `String` 错误通道里的能力缺失标记；SDK 将它还原为 `WireError::UNSUPPORTED`。
pub const UNSUPPORTED_PREFIX: &str = "UNSUPPORTED:";

pub fn unsupported(message: impl AsRef<str>) -> String {
    format!("{UNSUPPORTED_PREFIX}{}", message.as_ref())
}

pub fn split_unsupported(message: &str) -> Option<&str> {
    message.strip_prefix(UNSUPPORTED_PREFIX)
}

/// 可选扩展：旧 SDK 对该方法返回 `NO_METHOD`，不影响既有握手。
pub const SCHEDULES_LIST_METHOD: &str = "schedules.list";

/// 异步返回的简写。
pub type SendFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

/// 一个 provider 的静态描述（`init` 时申报）。
///
/// ── 与 [`proto::ProviderInfo`] 的关系 ───────────────────────
/// 这个结构是作者填的，`ProviderInfo` 是过线形态。分开是因为作者不该
/// 被迫构造 `ProviderId` 之类的协议类型 —— 填字符串更顺手。
#[derive(Clone, Debug)]
pub struct ProviderMeta {
    /// provider id（落盘契约，存量 id 不能改名）。
    pub id: String,
    /// 界面显示名。
    pub label: String,
    /// 注册表顺序（同值按 id 排）。
    pub order: i32,
    /// 静态能力位。
    pub capabilities: proto::Capabilities,
    /// 支持的登录方式。
    pub login_methods: Vec<proto::LoginMethod>,
    /// 内置的默认模型清单（没拉过远程目录时用它）。
    pub default_models: Vec<Value>,
    /// 这一家在界面上拥有的管理面板（空 = 没有）。
    pub admin_panels: Vec<proto::AdminPanel>,
    /// 这一家的账号行怎么渲染（`None` = 不申报，界面走兜底）。
    pub account_features: Option<proto::AccountFeatures>,
    /// 这一家的签到能力申报（`None` = 不申报，网关走内置兜底）。
    pub checkin: Option<proto::CheckinInfo>,
}

impl ProviderMeta {
    /// 最简构造：只有 id 与 label，其余全默认。
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            order: 0,
            capabilities: proto::Capabilities::default(),
            login_methods: Vec::new(),
            default_models: Vec::new(),
            admin_panels: Vec::new(),
            account_features: None,
            checkin: None,
        }
    }

    pub fn with_capabilities(mut self, capabilities: proto::Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn with_login(mut self, methods: Vec<proto::LoginMethod>) -> Self {
        self.login_methods = methods;
        self
    }

    pub fn with_models(mut self, models: Vec<Value>) -> Self {
        self.default_models = models;
        self
    }

    /// 声明这一家在界面上有哪些管理面板。
    ///
    /// ── 为什么必须申报（而不是让界面按 id 猜）─────────────────────
    /// 面板的实现在插件里（走 `admin.action`），渲染在网关界面。界面原先
    /// 按 provider id 写死「这家有哪些面板」，插件没装时照样显示，点进去
    /// 是一屏「读取失败」。申报之后，没装插件就没有这个 `ProviderInfo`，
    /// 界面也就不会画这些面板。
    ///
    /// ── 申报了却没实现会怎样 ────────────────────────────────────
    /// 面板照常显示，但每次读写都拿到 `admin.action` 的「不支持」错误 ——
    /// 与能力位同一条纪律：**只申报真的实现了的**。
    pub fn with_admin_panels(mut self, panels: Vec<proto::AdminPanel>) -> Self {
        self.admin_panels = panels;
        self
    }

    /// 申报这一家的**账号行渲染参数**（标识键、有效期键、余额入口……）。
    ///
    /// ── 为什么这属于插件而不是网关 ──────────────────────────────
    /// 账号表的每一列读哪个字段、要不要给余额入口、要不要拼地区后缀，
    /// 全部取决于上游事实。网关原先有一张按 provider id 写死的常量表，
    /// 加一家插件就得回头改网关；申报之后，网关只按字段渲染，新增一家
    /// 不必改界面。
    ///
    /// ── 怎么填 ──────────────────────────────────────────────────
    /// 以 [`proto::AccountFeatures::default()`] 起手，只打开这一家真有的事：
    /// `usage` 表示界面上给余额入口、`credit_detail` 表示后端会产出逐份
    /// `credits[]`、`identifier` / `expiry` 填**你自己 `account_view` 里写的
    /// 键名**（写错不会报错，只会让那一列显示「—」）。
    ///
    /// 不申报（不调用本方法）时网关走兜底表，旧行为不变。
    pub fn with_account_features(mut self, features: proto::AccountFeatures) -> Self {
        self.account_features = Some(features);
        self
    }

    /// 申报这一家的**签到能力**（签到中心据此分组、写链路说明、给按钮）。
    ///
    /// ── 为什么签到要单独申报 ────────────────────────────────────
    /// 签到是反代提供商独有的功能（自定义 API 提供商结构上不可能有），
    /// 而网关原先靠一张 17 项的常量表 + 一串按 id 的特判来决定「谁能签」。
    /// 申报之后，新插件装上，签到中心的分组与说明自动出现，网关一行不用改。
    ///
    /// ── 怎么填 ──────────────────────────────────────────────────
    /// 以 [`proto::CheckinInfo::default()`] 起手：
    ///   · `description` 写**这一家的链路怎么发积分**（一句话，给用户看）；
    ///   · `modes` 只在分档执行时才填（多数家只有 `[Checkin]`）；
    ///   · `onboarding` / `plans` / `growth` 表示签到中心里还有没有对应的那块；
    ///   · `ui_hooks` 只填客户端认得的钩子（见 [`proto::CheckinUiHook`]）。
    ///
    /// 不申报时网关走兜底：`Capabilities::checkin` 为真的家按「标准签到、
    /// 无说明」渲染。
    pub fn with_checkin(mut self, checkin: proto::CheckinInfo) -> Self {
        self.checkin = Some(checkin);
        self
    }

    /// 声明「这家认默认模型这个概念」（客户端没指定 `model` 时网关可注入
    /// `config.defaultModel`）。
    ///
    /// ── 不声明会怎样 ────────────────────────────────────────────
    /// 网关的口径是「默认值命中的 provider 里**有家认这个概念**才注入」。
    /// 不声明 → 请求按「未指定模型」发给上游 → 上游用它自己的默认模型，
    /// 而用户配置的 `defaultModel` 在这家上静默失效。
    ///
    /// 反过来**多报**更糟：网关会注入一个别家的模型名，那是一次必然 404
    /// 或答非所问的请求。所以只在实现里真的接受任意模型名时才声明。
    pub fn with_default_model(mut self) -> Self {
        self.capabilities.supports_default_model = true;
        self
    }

    /// 声明「这家有环境变量 / 默认登录态旁路」。
    ///
    /// ── 与 `anonymous_default_session` 的分工 ────────────────────
    /// `anonymous_default_session` 说的是「允许走那条旁路」，这一位说的是
    /// 「实现里有那份凭证的读取逻辑」。两者通常成对出现，但**不是同一个位**：
    /// 有的家允许匿名但凭证只能来自账号记录。
    ///
    /// 插件读不到网关进程的环境变量，所以这里申报的是「有旁路实现」；
    /// 「此刻真有凭证」由 `token.ensure` 的成败回答（那更准）。
    pub fn with_env_credentials(mut self) -> Self {
        self.capabilities.env_credentials = true;
        self
    }
}

/// 无状态转发：插件回答「这一次请求怎么构造」。
///
/// ── 为什么网关不自己拼 ──────────────────────────────────────
/// URL、鉴权头、请求体形状（WorkBuddy 要注入首条 system、Command Code 是
/// 自有 8 键信封、Trae 是白名单重建）全是 provider 知识。网关只负责发。
#[derive(Clone, Debug)]
pub struct ChatPlan {
    /// 上游完整 URL（含路径与查询串）。
    pub url: String,
    /// 请求头（不含出网代理相关；鉴权头在这里）。
    pub headers: Vec<(String, String)>,
    /// 已按本家规则改写过的请求体。
    pub body: Value,
    /// 上游响应说的是哪套协议。
    pub upstream: proto::UpstreamKind,
}

impl ChatPlan {
    /// 标准形态：上游说 OpenAI Chat。
    pub fn chat(url: impl Into<String>, headers: Vec<(String, String)>, body: Value) -> Self {
        Self { url: url.into(), headers, body, upstream: proto::UpstreamKind::Chat }
    }
}

/// 有状态转发的输出端：作者往这里推 head / chunk / end。
///
/// ── 为什么是「推」而不是「返回一个 Stream」───────────────────
/// 返回 Stream 要求 `serve()` 把作者的流包进一个 poll 循环，而作者那边的
/// 生命周期（持有上游连接、工具循环状态）与流的生命周期纠缠 —— 写起来
/// 极难。推模型让作者用最自然的 `loop { … sink.send(bytes).await … }`，
/// 背压由 `send` 的 await 天然表达（窗口满了它就停在那里）。
pub struct ConversationSink<'a> {
    inner: &'a mut dyn SinkInner,
}

impl<'a> ConversationSink<'a> {
    pub(crate) fn new(inner: &'a mut dyn SinkInner) -> Self {
        Self { inner }
    }

    /// 当前这条流的只读取消信号；可在等上游响应、子进程或回调时 select。
    /// 返回独立 Arc，观察取消不需要借用 sink，也不会取消同包其它请求。
    pub fn cancellation(&self) -> std::sync::Arc<crate::serve::Cancel> {
        self.inner.cancellation()
    }

    /// 上报上游响应头（**必须**在第一个 chunk 之前调一次）。
    pub async fn head(&mut self, head: proto::StreamHead) -> Result<(), String> {
        self.inner.send_head(head).await
    }

    /// 推一段字节（已是 OpenAI chat SSE 形态）。
    ///
    /// ── 背压在这里发生 ──────────────────────────────────────────
    /// 窗口用完时这个 await 会挂起，直到网关把额度还回来。作者不需要做
    /// 任何事 —— 但**不要**在推之前把整段回答缓冲到自己内存里，那会让
    /// 背压失去意义。
    pub async fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.inner.send_chunk(bytes).await
    }
}

/// `ConversationSink` 的内部接口（由 `serve()` 实现，作者不该用）。
pub trait SinkInner: Send {
    /// 自定义 sink 的默认信号永不触发，保持已有实现的源码兼容。
    fn cancellation(&self) -> std::sync::Arc<crate::serve::Cancel> {
        std::sync::Arc::new(crate::serve::Cancel::default())
    }

    fn send_head<'a>(&'a mut self, head: proto::StreamHead) -> SendFuture<'a, ()>;
    fn send_chunk<'a>(&'a mut self, bytes: &'a [u8]) -> SendFuture<'a, ()>;
}

/// 一家 provider 的完整实现。
///
/// ── 必答的两个方法 ──────────────────────────────────────────
/// [`Provider::meta`]（我是谁）与 [`Provider::provider_id`]（用哪个 id 认我）。
/// 其余全部有默认实现。
pub trait Provider: Send + Sync + 'static {
    /// 这个 provider 的静态描述。
    fn meta(&self) -> ProviderMeta;

    /// provider id（与 `meta().id` 相同；单独一个方法是为了让路由表能
    /// 在建表时不必构造整个 `ProviderMeta`）。
    fn provider_id(&self) -> String {
        self.meta().id
    }

    /// Optional process-owned cleanup. Existing providers keep their no-op default;
    /// helper-backed providers await this before the SDK acknowledges shutdown.
    fn shutdown<'a>(&'a self) -> SendFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    // ── 数据面 ────────────────────────────────────────────────

    /// 构造一次无状态请求。
    ///
    /// 默认返回错误：只有申报了 `capabilities.chat && !capabilities.stateful`
    /// 的家才该实现它。走到默认实现说明网关的分流坏了 —— 报错比发一个
    /// 语义不对的请求安全。
    fn build_chat<'a>(
        &'a self,
        _ctx: &'a crate::ctx::PluginCtx,
        _req: proto::ChatRequest,
    ) -> SendFuture<'a, ChatPlan> {
        Box::pin(async {
            Err(format!("{} 没有实现无状态转发（build_chat）", self.provider_id()))
        })
    }

    fn supports_image_generation(&self) -> bool {
        false
    }

    fn supports_image_editing(&self) -> bool {
        false
    }

    fn build_image_edit<'a>(
        &'a self,
        _ctx: &'a crate::ctx::PluginCtx,
        _req: crate::images::ImageRequest,
    ) -> SendFuture<'a, crate::images::ImagePlan> {
        Box::pin(async move {
            Err(unsupported(format!("{} 尚未接入图片编辑", self.provider_id())))
        })
    }

    fn build_image<'a>(
        &'a self,
        _ctx: &'a crate::ctx::PluginCtx,
        _req: crate::images::ImageRequest,
    ) -> SendFuture<'a, crate::images::ImagePlan> {
        Box::pin(async move {
            Err(unsupported(format!("{} 尚未接入图片生成", self.provider_id())))
        })
    }

    /// 执行一次有状态转发。
    ///
    /// 默认返回错误（同上）。申报了 `capabilities.stateful` 的家必须实现。
    fn conversation<'a>(
        &'a self,
        _ctx: &'a crate::ctx::PluginCtx,
        _req: proto::ConversationRequest,
        _sink: ConversationSink<'a>,
    ) -> SendFuture<'a, ()> {
        Box::pin(async {
            Err(format!("{} 没有实现会话式转发（conversation）", self.provider_id()))
        })
    }

    /// 上游错误分类。
    ///
    /// 默认 `Fatal` —— 与网关侧「未覆写即透传」的口径一致。
    fn classify(&self, _status: u16, _body: &Value) -> proto::ErrorClass {
        proto::ErrorClass::Fatal {
            status: _status,
            message: String::new(),
            upstream_code: None,
        }
    }

    /// 上游响应是否「其实是错误」（HTTP 200 但正文是失败话术）。
    fn is_error_response(&self, _status: u16, _headers: &BTreeMap<String, String>) -> bool {
        false
    }

    /// 流内拒绝标记（`finishes` = 内容策略，`markers` = 本账号额度）。
    fn refusal_markers(&self) -> (Vec<String>, Vec<String>) {
        (Vec::new(), Vec::new())
    }

    /// 可退避重试的建议；None 表示没有专属建议，允许网关使用通用规则。
    fn retry_advice(&self, _body: &Value, _attempt: usize, _budget: usize) -> Option<proto::RetryAdvice> {
        None
    }

    /// 可选的重试决定；Stop 终止本次请求，不再重发或换账号。
    fn retry_decision(
        &self,
        _status: u16,
        body: &Value,
        attempt: usize,
        budget: usize,
    ) -> crate::retry::RetryDecision {
        match self.retry_advice(body, attempt, budget) {
            Some(advice) => crate::retry::RetryDecision::Retry {
                delay_ms: advice.delay_ms,
                reason: advice.reason,
            },
            None => crate::retry::RetryDecision::Default,
        }
    }

    /// 思考档位翻译（默认不注入）。
    fn reasoning_patch(&self, _level: &str, _model: &str, _body: &Value) -> proto::ReasoningPatch {
        proto::ReasoningPatch::Skip { reason: "该提供商不支持思考等级".to_string() }
    }

    /// 限额冷却的连带模型（默认只有请求的那个）。
    fn cooldown_models(&self, _account: &str, _wire_model: &str) -> proto::CooldownModels {
        proto::CooldownModels::default()
    }

    // ── 凭证 ──────────────────────────────────────────────────

    /// 取一个现在能用的 token（含临期主动刷新）。
    ///
    /// `account` 为空串 = 用本家默认登录态（环境变量 / 桌面端实时读取）。
    fn ensure_token<'a>(
        &'a self,
        _ctx: &'a crate::ctx::PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, String> {
        Box::pin(async { Err("该提供商没有实现凭证获取".to_string()) })
    }

    /// 401 之后的无条件刷新。
    ///
    /// 默认转调 [`Self::ensure_token`] —— 但**没有续期能力的家应当保持
    /// 默认值**：网关不会对有状态家走这条链，而无状态家若真没续期接口，
    /// 转调也只是拿回同一个被拒的 token。
    fn refresh_token<'a>(
        &'a self,
        ctx: &'a crate::ctx::PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, String> {
        self.ensure_token(ctx, account)
    }

    /// 这个账号的凭证此刻算不算临期。
    ///
    /// 判不出来返回 `false`（宁可漏刷，也不要凭猜测打上游）。
    fn token_expiring<'a>(
        &'a self,
        _ctx: &'a crate::ctx::PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, bool> {
        Box::pin(async { Ok(false) })
    }

    // ── 目录 ──────────────────────────────────────────────────

    /// 当前模型清单。
    fn models<'a>(
        &'a self,
        _ctx: &'a crate::ctx::PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async {
            Ok(proto::ModelList {
                models: self.meta().default_models,
                meta: proto::CatalogMeta::default(),
            })
        })
    }

    /// 刷新模型目录。
    fn refresh_models<'a>(
        &'a self,
        _ctx: &'a crate::ctx::PluginCtx,
        _account: &'a str,
        _force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async { Ok(proto::RefreshOutcome::unchanged()) })
    }

    /// 可选的同次刷新快照信封；旧 provider 仍返回原 RefreshOutcome。
    /// helper-backed provider 可以避免宿主紧接 models.list 再出一次网。
    fn refresh_models_snapshot<'a>(
        &'a self,
        ctx: &'a crate::ctx::PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            serde_json::to_value(self.refresh_models(ctx, account, force).await?)
                .map_err(|_| "刷新结果无法序列化".to_string())
        })
    }

    // ── 余额 / 签到 / 领取 ─────────────────────────────────────

    /// 定时管理动作的申报：`[{id, enabled, times, action}]`。
    ///
    /// 默认值由插件提供，用户开关、时刻与执行记录由宿主持久化；未申报不调度。
    fn schedules(&self) -> Vec<Value> {
        Vec::new()
    }

    /// 余额 / 积分查询。
    fn usage<'a>(
        &'a self,
        _ctx: &'a crate::ctx::PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::UsageReport> {
        Box::pin(async { Err("该提供商没有实现余额查询".to_string()) })
    }

    /// 额度来源声明：只返回可核实的上游读数，旧宿主会忽略新增 JSON 字段。
    fn quota_sources(&self, report: &Value) -> Vec<Value> {
        report.get("quotaSources").and_then(Value::as_array).cloned().unwrap_or_default()
    }

    /// 每日签到。
    fn checkin<'a>(
        &'a self,
        _ctx: &'a crate::ctx::PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(async { Err("该提供商没有实现签到".to_string()) })
    }

    /// 旧宿主未传 manual 时按自动签到处理，保留各家的无人值守写请求限制。
    fn checkin_with_options<'a>(
        &'a self,
        ctx: &'a crate::ctx::PluginCtx,
        account: &'a str,
        _manual: bool,
    ) -> SendFuture<'a, crate::CheckinResult> {
        Box::pin(async move { self.checkin(ctx, account).await.map(Into::into) })
    }

    /// 领取类活动。
    fn claim<'a>(
        &'a self,
        _ctx: &'a crate::ctx::PluginCtx,
        _account: &'a str,
        _claim: &'a str,
    ) -> SendFuture<'a, Value> {
        Box::pin(async { Err("该提供商没有实现领取".to_string()) })
    }

    /// 领取类活动的**只读**状态（界面的「新手任务」页每次打开都要看进度）。
    ///
    /// 默认转调 [`Self::claim`] 会是很糟的默认：那等于「看一眼就替用户领一遍」。
    /// 所以这里默认报错，要求有进度的家显式实现。
    ///
    /// `refresh = true` 表示用户手动点了「查询任务」（跳过本地的结算记忆）。
    fn claim_status<'a>(
        &'a self,
        _ctx: &'a crate::ctx::PluginCtx,
        _account: &'a str,
        _refresh: bool,
    ) -> SendFuture<'a, Value> {
        Box::pin(async { Err("该提供商没有实现领取状态查询".to_string()) })
    }

    /// **厂商专属的管理动作**（成长任务 / 保活 / 平台福利 / 套餐领取…）。
    ///
    /// ── 为什么是一条通道而不是十几个具名方法 ────────────────────
    /// 见协议侧 [`proto::methods::gateway::ADMIN_ACTION`] 的说明：这些动作
    /// 彼此没有共性，参数与返回形状只有厂商自己知道。网关只做三件事 ——
    /// 把 `action` 与 `payload` 透下来、把结果原样传回、把错误翻成 HTTP。
    ///
    /// ── 默认实现 ────────────────────────────────────────────────
    /// 报错而不是返回空对象：静默成功会让界面显示「已完成」而实际什么都没做，
    /// 比明确报「不支持」糟得多。文案里带上动作名，便于用户/排障者指认。
    fn admin_action<'a>(
        &'a self,
        _ctx: &'a crate::ctx::PluginCtx,
        _account: &'a str,
        action: &'a str,
        _payload: &'a Value,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            Err(unsupported(format!("该提供商没有实现管理动作「{action}」")))
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    /// 规范化一条账号（手填 / 文件 / 桌面端导入）。
    fn normalize_account<'a>(
        &'a self,
        _ctx: &'a crate::ctx::PluginCtx,
        _input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async { Err("该提供商没有实现账号添加".to_string()) })
    }

    /// 从凭证重新生成公开视图。
    fn account_view(&self, _secret: &Value) -> Result<proto::AccountView, String> {
        Err("该提供商没有实现账号视图".to_string())
    }

    /// 账号的稳定身份（去重用）。
    fn account_identity(&self, _secret: &Value) -> Result<String, String> {
        Ok(String::new())
    }

    // ── 登录 ──────────────────────────────────────────────────

    /// 开始一次登录流程。
    fn login_start<'a>(
        &'a self,
        _ctx: &'a crate::ctx::PluginCtx,
        _method: &'a str,
        _payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(async { Err("该提供商没有实现登录".to_string()) })
    }

    /// 轮询登录进度（设备授权型流程用）。
    fn login_poll<'a>(
        &'a self,
        _ctx: &'a crate::ctx::PluginCtx,
        _flow: &'a str,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async { Err("该提供商没有实现登录轮询".to_string()) })
    }

    /// 回调换码（回调型流程用）。
    fn login_callback<'a>(
        &'a self,
        _ctx: &'a crate::ctx::PluginCtx,
        _callback: proto::LoginCallback,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async { Err("该提供商没有实现登录回调".to_string()) })
    }

    /// 取消一次登录流程。
    fn login_cancel<'a>(
        &'a self,
        _ctx: &'a crate::ctx::PluginCtx,
        _flow: &'a str,
    ) -> SendFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}
