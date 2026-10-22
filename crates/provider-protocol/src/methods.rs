//! 方法名常量与参数/返回的索引。
//!
//! ── 这个文件为什么只是常量与文档 ──────────────────────────────
//! 方法的**参数与返回类型**已经按面定义在各自的模块里（[`crate::chat`]、
//! [`crate::account`] 等）。这里只做一件事：给方法名一个唯一的事实来源，
//! 免得两端各写一遍字符串字面量 —— 拼错一个字母的症状是运行期
//! 「no method」，而它不会在编译期被发现。
//!
//! ── 双向对称：两个方向各有自己的方法集 ──────────────────────
//! 网关 → 插件（[`gateway`]）与插件 → 网关（[`plugin`]）是两套方法。
//! 名字空间刻意不重叠，收到不认识的方法时按 [`WireError::NO_METHOD`]
//! 回绝即可 —— 这样旧实现对端发来的新方法不会崩，只是那一条调用失败。

/// 网关发给插件的方法。
pub mod gateway {
    /// 握手：交换协议版本、`HostEnv`，返回 `Vec<ProviderInfo>`。
    ///
    /// ── 为什么握手就返回全部 provider ────────────────────────────
    /// 网关需要知道「这个包装了哪些家、各自的能力位」才能建路由表。
    /// 分多次问会让「装了一半」的中间态暴露给转发路径。
    pub const INIT: &str = "init";
    /// 存活探测（网关的健康检查用；插件应立刻回 `null`）。
    pub const PING: &str = "ping";
    /// 优雅关闭：插件停止接受新请求，等在途的完成后退出。
    pub const SHUTDOWN: &str = "shutdown";

    /// 无状态转发：参数 [`crate::chat::ChatRequest`]。
    ///
    /// ── 回复形态（两阶段）──────────────────────────────────────
    /// 先回一个 `result`，里面是「要发出去的 HTTP 请求」：
    /// `{url, headers, body, upstream}`。网关拿到之后**自己发**。
    ///
    /// 这与有状态路径的差别是刻意的：无状态家的重试、退避、账号轮换全在
    /// 网关侧，插件不该参与 —— 它只回答「怎么构造这一次请求」。
    pub const CHAT: &str = "chat";

    /// 有状态转发：参数 [`crate::chat::ConversationRequest`]。
    ///
    /// ── 回复形态（流式三阶段）──────────────────────────────────
    /// `head` → `chunk`* → `result: null`。插件自己发请求、自己解析上游，
    /// 只把「已经是 OpenAI chat SSE 形态」的字节吐给网关。
    pub const CONVERSATION: &str = "conversation";

    /// 取消一次在途的 `conversation`：参数 `{id}`（被取消调用的 id）。
    ///
    /// ── 为什么取消要单独一个方法而不是靠连接断开 ─────────────────
    /// 一个宿主进程服务多条并发流，连接不会断。必须能点名取消某一条。
    pub const ABORT: &str = "abort";

    /// 归还背压额度：参数 `{id, n}`。
    pub const CREDIT: &str = "credit";

    /// 上游错误的分类：参数 `{status, body}`，返回 [`crate::chat::ErrorClass`]。
    ///
    /// ── 为什么分类要过 RPC 而不是让插件在 chat 里一起给 ──────────
    /// 无状态路径里请求是**网关发的**，插件看不到响应。所以错误体回到插件
    /// 手里分类。这一条 RPC 每个错误响应才走一次，不在热路径上。
    pub const CLASSIFY: &str = "classify";

    /// 上游响应是否「其实是错误」（HTTP 200 但正文是失败话术）。
    /// 参数 `{status, headers}`，返回 `bool`。
    pub const IS_ERROR_RESPONSE: &str = "is_error_response";

    /// 流内拒绝标记：返回 `{finishes: [..], markers: [..]}`。
    ///
    /// ── 两组的区别（不要混）───────────────────────────────────
    /// `finishes` 判的是**内容策略**拒绝（不罚账号、换家）；
    /// `markers` 判的是**本账号额度**问题（必须罚账号）。
    pub const REFUSAL_MARKERS: &str = "refusal_markers";

    /// 退避重试建议：参数 `{body, attempt, budget}`，返回
    /// `Option<RetryAdvice>`。
    pub const RETRY_ADVICE: &str = "retry_advice";

    /// 思考档位翻译：参数 `{level, model, body}`，返回
    /// [`crate::chat::ReasoningPatch`]。
    pub const REASONING_PATCH: &str = "reasoning_patch";

    /// 限额冷却的连带模型：参数 `{account, wire_model}`，返回
    /// [`crate::chat::CooldownModels`]。
    pub const COOLDOWN_MODELS: &str = "cooldown_models";

    /// 取可用 access token：参数 `{account}`，返回 `{token}`。
    ///
    /// ── 为什么网关不自己存 token ────────────────────────────────
    /// 「临期主动刷新」的窗口与判据是 provider 知识（各家过期时间在哪个
    /// 字段、窗口多长都不同）。网关只要求「给我一个现在能用的」。
    pub const TOKEN_ENSURE: &str = "token.ensure";

    /// 401 之后的**强制**刷新：参数 `{account}`，返回 `{token}`。
    ///
    /// 与 `TOKEN_ENSURE` 的区别：那个是「临期才刷」，这个是「服务端刚
    /// 拒绝过，无条件刷」。合并成一个方法会让 401 后的重试拿到同一个
    /// 被拒的 token。
    pub const TOKEN_REFRESH: &str = "token.refresh";

    /// 这个账号的凭证此刻算不算临期：参数 `{account}`，返回 `bool`。
    ///
    /// 判不出来就返回 `false` —— 宁可漏刷，也不要凭猜测打上游。
    pub const TOKEN_EXPIRING: &str = "token.expiring";

    /// 模型清单：参数 `{account}`，返回 [`crate::model::ModelList`]。
    pub const MODELS_LIST: &str = "models.list";

    /// 刷新模型目录：参数 `{account, force}`，返回
    /// [`crate::model::RefreshOutcome`]。
    ///
    /// `force` 是「用户手动点了刷新」的语义：跳过缓存/TTL 真打一次上游。
    /// 复用自动路径的实现会让界面显示「刷新成功」而清单一个字没变。
    pub const MODELS_REFRESH: &str = "models.refresh";

    /// 余额 / 积分查询：参数 `{account}`，返回 [`crate::account::UsageReport`]。
    pub const USAGE_QUERY: &str = "usage.query";

    /// 签到：参数 `{account}`，返回 [`crate::account::CheckinReport`]。
    pub const CHECKIN_CLAIM: &str = "checkin.claim";

    /// 领取类活动（新手礼 / 每日任务 / 限时套餐）：
    /// 参数 `{account, claim}`，返回 `Value`。
    ///
    /// `claim` 是活动标识（各家自己命名），由 `provider.info` 的
    /// capabilities 之外的元数据申报 —— 见 SDK 的 `Provider::claims`。
    /// **空串 = 一键领取全部可领项。**
    pub const CLAIM: &str = "claim";

    /// 领取类活动的**只读**状态：参数 `{account}`，返回 `Value`。
    ///
    /// ── 为什么不能复用 `claim` ──────────────────────────────────
    /// `claim` 是写操作（会真的去上游领）。界面的「新手任务」页每次打开都要
    /// 看一眼进度，拿写操作去查一次等于每次进页面都替用户领一遍 —— 对
    /// 「上报即到账」那类家（Loomy）不只是浪费，还会让「用户没点按钮却已经
    /// 领了」这件事在台账里说不清。
    ///
    /// 返回形状由插件决定，与内置家各 `get_tasks` 同形（`api::onboarding`
    /// 直接透给前端）。
    pub const CLAIM_STATUS: &str = "claim.status";

    /// 规范化一条账号：参数 [`crate::account::NormalizeInput`]，
    /// 返回 [`crate::account::NormalizedAccount`]。
    pub const ACCOUNT_NORMALIZE: &str = "account.normalize";

    /// 从凭证重新生成公开视图（凭证更新后刷新界面）：
    /// 参数 `{secret}`，返回 [`crate::account::AccountView`]。
    pub const ACCOUNT_VIEW: &str = "account.view";

    /// 账号身份（去重用）：参数 `{secret}`，返回 `{identity}`。
    pub const ACCOUNT_IDENTITY: &str = "account.identity";

    /// 登录：开始。参数 `{method, payload, proxy}`，返回
    /// [`crate::account::LoginStart`]。
    pub const LOGIN_START: &str = "login.start";

    /// 登录：轮询。参数 `{flow}`，返回 [`crate::account::LoginPoll`]。
    pub const LOGIN_POLL: &str = "login.poll";

    /// 登录：回调。参数 [`crate::account::LoginCallback`]，
    /// 返回 [`crate::account::LoginPoll`]。
    pub const LOGIN_CALLBACK: &str = "login.callback";

    /// 登录：取消。参数 [`crate::account::LoginCancel`]，返回 `null`。
    pub const LOGIN_CANCEL: &str = "login.cancel";

    /// 管理面的**厂商专属动作**（成长任务 / 每日活跃保活 / 平台福利 / 套餐领取
    /// 这些「不是转发、也不是签到」的能力）。
    ///
    /// ── 为什么是一条通用通道而不是十几个具名方法 ────────────────
    /// 这些动作彼此之间没有共性：WorkBuddy 是任务树（查 / 接单 / 领奖），
    /// CodeArts 是运营活动（查活动位 / 领 / 确认），ZCode 是套餐激活。给每
    /// 一个都开一个协议方法，协议会随长尾厂商动作无限膨胀，而它们的参数形状
    /// 与返回形状**只有该厂商自己知道** —— 网关本来就不该理解它们。
    ///
    /// 所以形状定成：
    /// ```text
    ///   params = { provider, account, action: "<厂商自定义动作名>", payload: {...} }
    ///   result = 厂商自定义 JSON（网关原样透给前端）
    /// ```
    /// `action` 的取值域由插件自己定义（网关只当字符串透传）。
    ///
    /// ── 与 `claim` / `claim.status` 的界限 ──────────────────────
    /// 那两条是**跨家有共识**的语义（新手任务：查状态 / 领取），所以值得具名。
    /// 这里放的是没有共识的那些 —— 「猫猫旅行巡检一次」没法归约成 `claim`。
    pub const ADMIN_ACTION: &str = "admin.action";
}

/// 插件发给网关的方法（回调）。
///
/// ── 为什么账号读写要反向回调 ────────────────────────────────
/// 账号库在网关侧（单文件 SQLite，要参与整库备份、云同步、并发控制）。
/// 让插件自己开一份库会让「备份恢复」漏掉插件那份数据 —— 那是不可接受的。
/// 所以插件通过这几个方法读写，网关保证事务与并发语义。
///
/// ── 死锁风险（实现方必读）───────────────────────────────────
/// 插件的读循环**必须**只负责派发、绝不在读循环里 await 业务逻辑。
/// 否则：插件处理一个 chat 时回调 `store.get`，而读循环正卡在等这个 chat
/// 完成 —— 自己把自己锁死。SDK 的 `serve()` 已经按这个原则写。
pub mod plugin {
    /// 读一条账号：参数 `{provider, account}`，返回 `{secret, public}`。
    ///
    /// `account` 为空串 = 该 provider 的队首账号。
    pub const STORE_GET: &str = "store.get";
    /// 列出一个 provider 的全部账号：参数 `{provider}`，
    /// 返回 `[{id, secret, public}]`。
    pub const STORE_LIST: &str = "store.list";
    /// 条件更新凭证：参数 `{account, expect, secret}`，返回 `{updated}`。
    ///
    /// ── 「条件」是什么意思 ──────────────────────────────────────
    /// `expect` 是插件读到的那个 `secret` 的指纹（或某个字段）。网关只有在
    /// 当前值仍等于它时才写入 —— 这是「比较再写」，防止两个并发刷新互相
    /// 覆盖（今天各家 `update_*_if_current` 的语义）。
    pub const STORE_UPDATE: &str = "store.update";
    /// 标记账号对某模型限额：参数 `{account, model, reset_at}`，返回 `null`。
    pub const STORE_MARK_LIMITED: &str = "store.mark_limited";
    /// 读一份台账（CodeArts 福利这类）：参数 `{account, kind}`，返回 `Value`。
    pub const STORE_LEDGER_GET: &str = "store.ledger_get";
    /// 写一份台账：参数 `{account, kind, value}`，返回 `null`。
    pub const STORE_LEDGER_PUT: &str = "store.ledger_put";
    /// 读一条按家配置（例如 CatPaw 的 token2）：参数 `{provider, key}`，
    /// 返回 `Value`。
    pub const SETTINGS_GET: &str = "settings.get";
    /// 写一条按家配置：参数 `{provider, key, value}`，返回 `null`。
    pub const SETTINGS_SET: &str = "settings.set";

    /// 日志：参数 `{level, message}`，无返回。
    ///
    /// ── 为什么日志要回传而不是让插件自己打 stderr ────────────────
    /// 网关的运行日志页按级别过滤、按时间排序、有容量上限。插件直接写
    /// stderr 会绕过这三件事（而且 stderr 在 Windows GUI 程序里根本没有
    /// 控制台可写）。stderr 只留作「插件崩了」这类兜底诊断。
    pub const LOG: &str = "log";

    /// 通知界面（顶部提示 / 账号状态变化）：参数 `{kind, message}`。
    ///
    /// `kind`：`notice`（中性提示）、`warn`（警告）、`auth_changed`
    /// （某账号的登录态变了，界面要刷新）、`progress`（长任务进度）。
    pub const NOTIFY: &str = "notify";
}
