//! WorkBuddy 提供商（国内版 / 国际版两家）：数据面 + 账号面。
//!
//! ── 本目录里有什么 ──────────────────────────────────────────
//! ```text
//!   mod.rs          Provider 实现（能力位、build_chat、classify、续期、余额）
//!   region.rs       两个地区（provider id / 端点 / 客户端身份 / 环境变量旁路）
//!   auth.rs         账号身份头、临期判定与单次续期
//!   refresh.rs      单飞续期、凭证复查与比较再写
//!   refresh_identity.rs  续期后重新核对账号、企业与授权身份
//!   login.rs        浏览器登录及地区请求上下文
//!   catalog.rs      公共配置与企业模型目录刷新
//!   catalog_access.rs  按账号身份保存和校验模型权限
//!   catalog_state.rs   两地展示目录与持久快照恢复
//!   image_generation.rs  文生图 / 参考图编辑（`/v2/images/*`）
//!   normalize.rs    出站归一（developer→system / tool_choice / image_url / …）
//!   reasoning.rs    目录驱动的推理档位与 DeepSeek 历史兼容
//!   outbound.rs     对话头、URL 与会话折叠
//!   contract.rs     官方请求框架、代理与可选网络策略
//!   errors.rs       业务错误分类与通用处置策略
//!   web_session.rs  按账号绑定的私有官网 Cookie
//!   billing/        新版积分汇总、资源包与企业额度
//!   quota_reset.rs  429 文案里的「恢复时间」解析
//! ```
//!
//! ── 与网关那份的差别（迁移时改的，逐条）───────────────────────
//! | 网关 | 插件 |
//! |---|---|
//! | `AccountStore` + `AuthService` | `PluginCtx` + [`crate::store`]（账号库在网关） |
//! | `core::models::global_catalog(region)` 进程级目录 | 本模块的 `CatalogState` 槽（清单由网关的 `rpc_cache` 持久化） |
//! | `catalog_cache` 落盘 | 网关侧负责（插件重启后由网关重放） |
//! | `model_rules::seed_workbuddy_defaults` | 网关侧负责（模型规则是用户配置） |
//! | `needsRelogin` 标记 / 12153 连续计数 | **不迁**：插件只如实报错，由编排层退避（见 `refresh`） |
//! | 重试间隔读设置页 | 硬编码 10 秒（插件没有配置回调） |
//! | `LoginService::start` 后台任务 while 轮询 | [`login`]：`login.start` 只做 ①②，`login.poll` 每次问一次 |
//! | 每日签到 | 本家没有签到（`checkin: false`） |
//!
//! ── 已迁的能力 ──────────────────────────────────────────────
//! `chat` / `refresh`（token 续期）/ `model_refresh`（`/v3/config`）/
//! `usage`（积分简报 + 明细 + 套餐）/ `anonymous_default_session`
//! （`WORKBUDDY_TOKEN` 旁路）/ `sse_strip_newline_chunks`（上游会插整片换行的
//! 节拍）/ `web_login`（轮询型浏览器登录）。
//!
//! ── 国内版 / 国际版 = 两个实例 ───────────────────────────────
//! 两端协议完全相同，差别只在站点与客户端身份（UA / platform / 产品名），
//! 以及**各自独立的模型目录槽**。完整理由见 [`region`] 的模块头。
//!
//! ── 默认登录态（`WORKBUDDY_TOKEN` 旁路）─────────────────────
//! workbuddy 独有的「一个账号都没有时也能转发」能力（脚本 / CI 用户）：
//! `anonymous_default_session: true` + `ensure_token` 的空 account 分支。
//! 这是**行为兼容**的一部分 —— 去掉它会让 `WORKBUDDY_TOKEN` 用户的
//! `/v1/chat/completions` 直接 401。变量按地区分开（国内 `WORKBUDDY_TOKEN`、
//! 国际 `WORKBUDDY_INTL_TOKEN`，旧组合 `WORKBUDDY_EDITION=intl` 仍可用）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件在对话链路上，**绝不** unwrap/expect/panic。

mod accounts;
mod admin;
mod auth;
mod billing;
mod builtin;
mod catalog;
mod catalog_access;
mod catalog_state;
mod contract;
mod errors;
mod growth;
mod image_generation;
mod keepalive;
mod login;
mod normalize;
mod outbound;
mod quota_reset;
mod reasoning;
mod refresh;
mod refresh_identity;
mod region;
mod search;
mod web_session;

pub use region::Region;
pub(super) use outbound::{chat_completions_url, chat_headers, session_endpoint_of, session_of_record};
use outbound::{ensure_leading_system_message, DEFAULT_SYSTEM_PROMPT};

// ─── 以下为 Provider 实现（原 `adapter.rs` 的正文）──────────────

use serde_json::Value;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ChatPlan, PluginCtx, Provider, ProviderMeta, SendFuture};

use crate::errors::GatewayError;

/// 上游错误码 11-128（历史文案：Illegal API invocation from an unapproved channel）。
///
/// 实测语义：多为提示词命中上游敏感词审核而被拦截（并非单纯的频率风控）。
/// 常量名沿用 Node 版的历史命名，不要据此理解成「频率限制」。
/// 拦截会持续一小段时间，期间客户端失败自动重试会形成重试风暴并给拦截续期，
/// 因此命中 11-128 时值得退避重试 —— 次数与间隔统一走设置页的「请求重试」
/// （历史硬编码是 10 秒 / 25 秒各一次；间隔设得过短会拉长拦截窗口，建议 ≥10 秒）。
const RATE_LIMIT_CODE: i64 = 11_128;

/// WorkBuddy 适配器（无状态单例，见 `adapter::adapter_for`）。
///
/// 唯一的字段是地区：两端协议一致，地区决定端点兜底、客户端身份（UA /
/// platform / 产品名）与**要读哪一份模型目录**。
pub struct WorkBuddyProvider {
    /// 本实例服务的地区（国内版 / 国际版）
    region: Region,
}

impl WorkBuddyProvider {
    /// 构造某个地区的适配器（`const`：两个实例都是静态量）
    pub const fn new(region: Region) -> Self {
        Self { region }
    }

    /// 取一条账号的**会话形态**（`auth` / `account` 两个子对象 + `endpoint`）。
    ///
    /// ── 为什么要把扁平 secret 折回会话形态 ────────────────────
    /// 网关的账号记录本身就是会话形状（`account_store::session_from_record` 把
    /// 记录上的字段收进 `account` / `auth` 两个子对象）。插件拿到的 secret 是
    /// **扁平的**（各家自定义），而 `build_auth_headers` / `normalize` / billing
    /// 都按会话形状读。所以这里做一次折叠，让那些函数逐字可用。
    async fn session_of(&self, ctx: &PluginCtx, account: &str) -> Result<Value, GatewayError> {
        session_for(ctx, self.region, account).await
    }

    async fn refresh_account(
        &self,
        ctx: &PluginCtx,
        account: &str,
        record: &Value,
    ) -> Result<String, GatewayError> {
        refresh::refresh_account(ctx, self.region, account, record).await
    }
}

impl WorkBuddyProvider {
    /// 国内版实例
    pub const CN: Self = Self::new(Region::Cn);
    /// 国际版实例（拆家后与国内版各自独立，见模块头）
    pub const INTL: Self = Self::new(Region::Intl);
}

/// 取某个地区某条账号的会话形态。
///
/// 与 [`WorkBuddyProvider::session_of`] 是同一段逻辑的自由函数版：图片链路
/// （[`image_generation`]）拿不到 `&self`，只能按 `region` 参数化复用 ——
/// 复制一份到那边迟早会分叉（`WORKBUDDY_TOKEN` 旁路只在一处生效那种）。
pub(super) async fn session_for(
    ctx: &PluginCtx,
    region: Region,
    account: &str,
) -> Result<Value, GatewayError> {
    if account.is_empty() {
        if let Some(token) = region.env_token() {
            return Ok(serde_json::json!({
                "auth": { "accessToken": token },
                "account": {},
                "edition": region.id(),
            }));
        }
    }
    let record = crate::store::record(ctx, region.provider_id(), account).await?;
    Ok(session_of_record(&record, region))
}

impl Provider for WorkBuddyProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(
            self.region.provider_id(),
            format!("WorkBuddy {}", self.region.label()),
        )
        .with_capabilities(proto::Capabilities {
            chat: true,
            stateful: false,
            // `/auth/token/refresh` + `X-Refresh-Token` 头（见 `auth`）
            refresh: true,
            // 远程目录：`GET /v3/config` 的 `data.models`
            model_refresh: true,
            // 腾讯计费接口（见 `billing`）
            usage: true,
            // 国内版有每日签到（`billing::checkin`）；国际版走「每日活跃」那条
            // 链路（`keepalive` + `admin` 的 `activity.run`），没有普通签到接口。
            // 能力位是**实例级**的，所以按地区分开。
            checkin: self.region == Region::Cn,
            claims: false,
            // 浏览器登录（轮询型：授权页在上游域名终结，回调永不回本机）
            web_login: true,
            sse_model_rewrite: false,
            // 上游会在相邻正文分片之间插「整片只有换行」的节拍（实测 34.5%）
            sse_strip_newline_chunks: true,
            // `WORKBUDDY_TOKEN` / `WORKBUDDY_INTL_TOKEN` 旁路：一个账号都没有时
            // 也能转发（脚本 / CI 用户的常规用法）
            anonymous_default_session: true,
            system_proxy_when_unset: false,
            refresh_uses_account: true,
            // config.json 的 `defaultModel`（缺省 auto）：客户端未指定 model 时
            // 网关据此注入 —— 与内置实现逐字一致（唯一覆写这一位的家）
            supports_default_model: true,
            // `WORKBUDDY_TOKEN` / `WORKBUDDY_INTL_TOKEN` 旁路实现存在
            env_credentials: true,
        })
        .with_login(vec![proto::LoginMethod::Browser {
            button: "网页登录".to_string(),
            hint: Some(
                "在打开的页面里登录 WorkBuddy 账号，完成后本页会自动继续".to_string(),
            ),
        }])
        .with_models(catalog_state::list(self.region).models)
        // 面板按地区分开：两版的上游玩法完全不同。
        //   国内版：成长任务树（任务 / 开学季 / 猫猫旅行）与签到
        //   国际版：每日活跃保活（模型链 + 定时排程），没有普通签到接口
        .with_admin_panels(match self.region {
            Region::Cn => vec![proto::AdminPanel::GrowthTasks],
            Region::Intl => vec![proto::AdminPanel::DailyKeepalive],
        })
        // 账号行：两地区能力一致（账号字段、余额口径、续期语义都相同）。
        //   `credit_detail` —— `query_usage` 在既有积分简报之外还出 `credits[]`
        //     （逐份积分包的面额 / 已用 / 剩余 / 领取 / 到期）。enterprise 账号
        //     没有资源包、后端不挂 `credits`，界面按空数组自然退回原样。
        //   `credit_expiry` —— 每份包的 `DeductEndTime` 写成 `credits[].expireAt`
        //     （毫秒），所以「快到期优先」排序对它可用。
        //   `edition` —— 只有国内版开：国际版的注册名「WorkBuddy 国际版」
        //     自带地区，再拼一次会得到「WorkBuddy 国际版 国际版」。
        .with_account_features(proto::AccountFeatures {
            usage: true,
            credit_detail: true,
            credit_expiry: true,
            edition: self.region == Region::Cn,
            identifier: vec!["uid".to_string()],
            expiry: vec!["expiresAt".to_string()],
            ..Default::default()
        })
        .with_checkin(proto::CheckinInfo {
            // 两地区共用一份申报：国内版是普通签到，国际版是「每日活跃」
            // （保活点亮日活、领取才加积分），所以国际版多两档执行方式。
            //   `growth` 只有国内版：国际版没有 CN 任务中心。
            description: Some(match self.region {
                Region::Cn => "腾讯每日签到接口 · 仅国内版".to_string(),
                Region::Intl => "每日活跃任务 · 网页会话保活点亮日活，领取才加积分 · 两者可分开执行".to_string(),
            }),
            modes: match self.region {
                Region::Cn => vec![proto::CheckinMode::Checkin],
                Region::Intl => vec![
                    proto::CheckinMode::Keepalive,
                    proto::CheckinMode::Claim,
                    proto::CheckinMode::Full,
                ],
            },
            growth: self.region == Region::Cn,
            ..Default::default()
        })
    }

    fn schedules(&self) -> Vec<Value> {
        match self.region {
            Region::Cn => vec![serde_json::json!({
                "id": "travel",
                "enabled": true,
                "times": ["09:00", "21:00"],
                "action": "growth.travel.batch",
            })],
            Region::Intl => Vec::new(),
        }
    }

    fn models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async move { Ok(catalog_state::current(ctx, self.region).await) })
    }

    fn reasoning_patch(&self, level: &str, model: &str, body: &Value) -> proto::ReasoningPatch {
        let model = catalog_state::model(self.region, model);
        reasoning::patch(level, model.as_ref(), body)
    }

    fn reasoning_patch_async<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        level: &'a str,
        model: &'a str,
        body: &'a Value,
    ) -> SendFuture<'a, proto::ReasoningPatch> {
        Box::pin(async move {
            catalog_state::current(ctx, self.region).await;
            Ok(self.reasoning_patch(level, model, body))
        })
    }

    fn outbound_reasoning(&self, body: &Value) -> Option<Option<String>> {
        let id = body.get("model").and_then(Value::as_str).unwrap_or("");
        let model = catalog_state::model(self.region, id);
        Some(reasoning::outbound_level(body, model.as_ref()))
    }

    /// 构造 `POST {endpoint}/v2/chat/completions` 的请求（头集合与 URL 逐字沿用
    /// 改造前 `upstream::request` 的实现）。
    ///
    /// `account` 是账号会话形态：`core::auth` 的 `build_auth_headers` 直接读
    /// `auth.accessToken` / `account.uid` / `auth.domain` 等字段即可，
    /// 与改造前 `session_for` 拿到的对象完全同形。
    ///
    /// ── 出站归一化的位置与顺序（2026-09 新增）─────────────────────
    /// 本函数是「**这一家的**请求即将发出」的唯一汇聚点（`provider_loop` 对同一家
    /// 同池的重试复用同一份 body，见 `send_cache`），所以本家上游的形态要求
    /// （role 白名单 / tool_choice 只认字符串 / image_url 只认对象 / 只认
    /// max_tokens）在这里一次改对 —— 换了账号也不会重新踩同一批 400。
    ///
    /// **归一化必须在 `ensure_leading_system_message` 之前**：客户端用
    /// `developer` 打头时，先归一成 `system` 才判得出「首条已是 system」；
    /// 反过来会多注入一条兜底 system，而上游对多 system 的行为未实测。
    fn build_chat<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ChatRequest,
    ) -> SendFuture<'a, ChatPlan> {
        Box::pin(async move {
            let mut session = self.session_of(ctx, &req.account).await?;
            let proxy = match req.proxy.clone() {
                Some(proxy) => Some(proxy),
                None if agent2api_provider_sdk::extensions::direct_proxy_override() => None,
                None => contract::proxy_of(&session).map_err(GatewayError::into_control_message)?,
            };
            session["proxy"] = serde_json::json!(proxy);
            let model = catalog_access::check(ctx, self.region, &session, &req.wire_model, proxy.as_ref()).await
                .map_err(GatewayError::into_control_message)?;
            let (mut normalized, report) = normalize::normalize_outbound(&req.body, &session);
            reasoning::normalize_outbound(&mut normalized, model.as_ref());
            if report.notable() {
                crate::logging::verbose(
                    "[Upstream]",
                    &format!("WorkBuddy 出站归一：{}", report.describe()),
                );
            }
            let with_system = match ensure_leading_system_message(&normalized) {
                Some(next) => {
                    crate::logging::verbose(
                        "[Upstream]",
                        &format!("首条消息非 system，已注入系统消息：{DEFAULT_SYSTEM_PROMPT}"),
                    );
                    next
                }
                None => normalized,
            };
            let url = chat_completions_url(&session, self.region);
            let headers = chat_headers(&session, self.region, Some("text/event-stream"));
            Ok(ChatPlan::chat(url, headers, with_system))
        })
    }

    /// 图片通道：`/v2/images/generations` 与 `/v2/images/edits`（见 [`image_generation`]）。
    ///
    /// 能力位是**实例级**的（两个地区同一套实现），但哪条模型能出现在哪条通路上
    /// 由目录里那条模型的 `tags` 决定（`text-to-image` / `image-to-image`）：
    /// 于是「上游给某个地区下发什么图像模型」这件事跟着账号自己的 `/v3/config`
    /// 走，不需要在代码里维护 id 白名单。
    fn supports_image_generation(&self) -> bool {
        true
    }

    fn supports_image_editing(&self) -> bool {
        true
    }

    fn build_image<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: agent2api_provider_sdk::images::ImageRequest,
    ) -> SendFuture<'a, agent2api_provider_sdk::images::ImagePlan> {
        Box::pin(image_generation::build(ctx, req, self.region, false))
    }

    fn build_image_edit<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: agent2api_provider_sdk::images::ImageRequest,
    ) -> SendFuture<'a, agent2api_provider_sdk::images::ImagePlan> {
        Box::pin(image_generation::build(ctx, req, self.region, true))
    }

    /// 上游错误分类（照抄改造前 `upstream` 的判定与文案）：
    ///   - 401 → TokenExpired（刷新后同账号重试一次）
    ///   - 429 或 code 6004 → QuotaLimited（冷却 + 换账号）
    ///   - 内容策略拦截（11-128 或审核文案）→ ContentBlocked（**不罚账号**：换中性提示词后
    ///     同账号重试一次 + 触发降级，见 `core::degrade`）
    ///   - 其余 → Fatal（原样透传）
    ///
    /// 文案逐字对齐改造前 `rotate::request_with_waf_retry` 的那三行
    /// `format!("上游返回 {status}: {message}{hint}")`。
    fn chat_policy(&self, _body: &Value) -> Value {
        serde_json::json!({ "repairNativeTools": true, "stableConversation": true, "network": contract::network_policy(self.region) })
    }

    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        errors::classify(status, body)
    }

    fn error_policy(&self, status: u16, body: &Value) -> Value {
        errors::policy(status, body)
    }

    fn retry_decision(&self, status: u16, body: &Value, attempt: usize, budget: usize) -> agent2api_provider_sdk::RetryDecision {
        let decision = errors::retry(status, body, attempt, budget);
        if !matches!(decision, agent2api_provider_sdk::RetryDecision::Default) { return decision; }
        match self.retry_advice(body, attempt, budget) {
            Some(advice) => agent2api_provider_sdk::RetryDecision::Retry { delay_ms: advice.delay_ms, reason: advice.reason },
            None => agent2api_provider_sdk::RetryDecision::Default,
        }
    }

    fn is_error_response(&self, status: u16, headers: &std::collections::BTreeMap<String, String>) -> bool {
        !(200..300).contains(&status) || headers.get("content-type")
            .is_some_and(|value| value.to_ascii_lowercase().contains("application/json"))
    }


    /// 取 access token（含临期主动刷新并回写账号）。
    ///
    /// - `account` 非空 → 该账号的 token；临期且可刷新时自动刷新。
    /// - `account` 为空 → 本地区的默认登录态（环境变量 `WORKBUDDY_TOKEN` /
    ///   `WORKBUDDY_INTL_TOKEN`，其次队首账号）。
    ///
    /// ── 与网关那份的差别（迁移时改的）──────────────────────────
    /// 网关调 `AuthService::refresh_account`（那个服务还管 needsRelogin 标记、
    /// 12153 连续计数等**库侧状态**）。插件这边库在网关，所以只做「读记录 →
    /// 临期判 → 单飞续期 → 写回」这一条链，见 [`refresh::refresh_account`]。
    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            if account.is_empty() {
                return default_session_token(ctx, self.region).await;
            }
            let record = crate::store::record(ctx, self.region.provider_id(), account).await?;
            if record.is_null() {
                return Err(format!("账号 {account} 没有可用凭证"));
            }
            let session = session_of_record(&record, self.region);
            let expires_at = session
                .get("auth")
                .and_then(|auth| auth.get("expiresAt"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let refresh_token = session
                .get("auth")
                .and_then(|auth| auth.get("refreshToken"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // 临期主动刷新（改造前 `auth::get_current_session` 的行为）
            if auth::is_token_expiring(expires_at) && !refresh_token.is_empty() {
                crate::logging::verbose(
                    "[Auth]",
                    &format!("token 临期，自动刷新（账号 {account}）…"),
                );
                match self.refresh_account(ctx, account, &record).await {
                    Ok(token) => return Ok(token),
                    // 续期失败不阻断转发：拿现有的 token 试一次（编排层会在
                    // 401 时走 refresh_token 那条强制刷新）
                    Err(error) => crate::logging::verbose(
                        "[Auth]",
                        &format!("临期续期失败（沿用现有 token）：{}", error.message),
                    ),
                }
            }
            let token = session
                .get("auth")
                .and_then(|auth| auth.get("accessToken"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if token.is_empty() {
                return Err(format!("账号 {account} 的 accessToken 为空"));
            }
            Ok(token)
        })
    }

    /// 401（token 被服务端拒绝）后的**强制**刷新：不看临期窗口，直接续期。
    ///
    /// ── 为什么必须与 `ensure_token` 分开 ────────────────────────
    /// `ensure_token` 只在「临期」时才刷新，而 401 完全可能发生在一个时间上还
    /// 很新的 token 上（服务端侧失效、账号被顶下线、refreshToken 轮换）。此时
    /// 只调 ensure 会拿回同一个被拒的 token，编排层的「刷新后同账号重试一次」
    /// 就退化成「用同一个坏 token 再打一次」。
    ///
    /// 空 `account`（默认登录态）无账号记录可续期：环境变量凭证是静态的，
    /// 直接返回错误由编排层按普通失败处理（只重试一轮，不会空转）。
    fn refresh_token<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, String> {
        Box::pin(async move {
            if account.is_empty() {
                return Err(
                    "默认登录态的凭证无法续期（环境变量凭证是静态的），请重新登录".to_string(),
                );
            }
            let record = crate::store::record(ctx, self.region.provider_id(), account).await?;
            if record.is_null() {
                return Err(format!("账号 {account} 不存在"));
            }
            self.refresh_account(ctx, account, &record)
                .await
                .map_err(GatewayError::into_control_message)
        })
    }

    /// 刷新**本地区**的模型目录（`GET /v3/config` 的 `data.models`）。
    ///
    /// ── 与网关那份的差别（迁移时改的）──────────────────────────
    /// 网关调 `ModelCatalog::refresh_with_current_account`（那个类型还管落盘
    /// 缓存、启动加载、目录合并）。插件只做「拿一份账号 → 打一次 `/v3/config`
    /// → 解析 → 存进本地槽」，见 [`catalog::refresh`]。
    ///
    /// ── `force` 对本家无差别（有意）─────────────────────────────
    /// workbuddy 的 `/v3/config` 拉取**没有缓存/TTL 早退**：每次调用都真打上游。
    /// 因此 `force` 的「绕过缓存」语义在本家无事可绕 —— 自动路径与手动路径的
    /// 行为逐字相同，直接忽略该参数（加了本地节流反而会让用户按下按钮后拿到
    /// 一份没动的清单，那正是给 `force` 存在的理由）。
    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        _force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            let outcome = catalog::refresh(ctx, self.region, account).await;
            if outcome.refreshed {
                ctx.notify_models_changed(self.region.provider_id(), account).await.ok();
            }
            Ok(if outcome.refreshed {
                proto::RefreshOutcome::refreshed(outcome.count())
            } else {
                // 本家没有「TTL 跳过」这一档：没刷就是真没拿到（网络 / 空清单 /
                // 没有登录态），全部如实报成失败原因，界面据此显示「为什么没变」
                proto::RefreshOutcome::failed(outcome.reason)
            })
        })
    }

    /// 11-128 退避建议（改造前 `rotate::request_with_waf_retry` 的判定；
    /// 次数与间隔改由设置页的「请求重试」统一提供）。
    ///
    /// **循环**留在编排层（架构文档 §4.3「11-128 退避逻辑保持在转发层」），
    /// 这里只回答「这个错误要不要退避、退多久、为什么」——
    /// 「哪个码是敏感词拦截」是 workbuddy 的知识，不该漏进 `upstream/`。
    ///
    /// `budget` 是编排层按「这一轮是不是已经换过家」算出的预算
    /// （见 `ProviderAdapter::retry_advice` 的说明）—— 本函数只比 `attempt`
    /// 与它，不去读全局设置里的哪一档。
    ///
    /// 返回的 `reason` 不含「第 n/N 次」：那是编排层才知道的读数
    /// （见 `RetryAdvice` 的说明）。
    fn retry_advice(
        &self,
        error_body: &Value,
        attempt: usize,
        budget: usize,
    ) -> Option<proto::RetryAdvice> {
        let code = error_body.get("code").and_then(Value::as_i64);
        if code != Some(RATE_LIMIT_CODE) {
            return None;
        }
        if attempt >= budget {
            return None;
        }
        // ── 重试间隔在插件里取不到设置页的值（迁移时改的一处）──────────
        // 网关读 `config::retry_settings().delay_ms()`（设置页「请求重试」）。
        // 插件没有配置回调，所以用**历史硬编码**那一档：10 秒（改造前的默认，
        // 也是设置页的出厂值）。影响面：用户把间隔改过之后，插件路径上仍按
        // 10 秒退避 —— 比不重试好，比读配置差一点。
        Some(proto::RetryAdvice {
            delay_ms: 10_000,
            reason: "上游敏感词拦截（11-128），请检查提示词中的敏感词".to_string(),
        })
    }

    /// 临期判定：读该账号**会话里的** `auth.expiresAt`（不是 accounts.json 的
    /// 公开形态 —— 那份按设计只有 `tokenTail`），窗口用 `auth::is_token_expiring`
    /// 里那同一个 5 分钟常量。
    ///
    /// ── 为什么复用 `is_token_expiring` 而不是自己比一个数 ──────────
    /// 转发链路的懒刷新（`ensure_access_token`）用的就是它；维护任务若另写一个
    /// 窗口，会出现「维护认为不需要刷、请求链路却认为临期」的分叉 ——
    /// 两边都「没坏」，只是判断不一致，排障时极难发现。
    ///
    /// 取不到会话（账号不存在 / 已无凭证）时返回 false（见 trait 契约：
    /// 判不出来就不刷）。
    fn token_expiring<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, bool> {
        Box::pin(async move {
            if account.is_empty() {
                return Ok(false);
            }
            let Ok(record) = crate::store::record(ctx, self.region.provider_id(), account).await
            else {
                return Ok(false);
            };
            let session = session_of_record(&record, self.region);
            // 没有 refreshToken 的账号刷不了 —— 判成「不临期」（与网关同口径：
            // 判不出来就不刷）
            if session
                .get("auth")
                .and_then(|auth| auth.get("refreshToken"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty()
            {
                return Ok(false);
            }
            let expires_at = session
                .get("auth")
                .and_then(|auth| auth.get("expiresAt"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            Ok(auth::is_token_expiring(expires_at))
        })
    }

    /// 查积分简报（**既有五键逐字保留**）+ 逐份积分包明细。
    ///
    /// ── 为什么既有形状不归一化（这是硬要求）─────────────────────
    /// `{kind, unlimited, totalLeft, planLeft, bonusLeft}` 是改造前就有的既有契约，
    /// 前端一直按它渲染（余额列的读数，见 `ui/accounts-table.js` 的 `usageSummary`，
    /// 后端 `usage_records::extract_remaining` 也认 `totalLeft` 这一个键）。
    /// 把它包装成 trait 文档里那套 `{available, unit, wallets, subscription}`
    /// 会让那一格的显示退化 —— 那是明令禁止的。所以这里是**加**不是换：
    /// 五个既有键由 `billing::credits_summary_from` 原样产出（与
    /// `query_credits_summary` 同一份逻辑），新挂上去的只有两项：
    ///
    ///   * `credits[]`      —— 逐份积分包明细（面额 / 已用 / 剩余 / 领取 / 到期），
    ///                        账号页的「积分明细」弹层读它（能力位 `creditDetail`）；
    ///   * `subscription`   —— 当前生效套餐的名字与到期时刻，弹层头部那一行。
    ///
    /// 成长在一处、退化在另一处才叫「不改形状」：既有键一个不动、读数与悬停提示
    /// 逐字不变，新增的两个键只有弹层在读。
    ///
    /// ── 为什么是 `query_usage` 而不是 `query_credits_summary` ─────
    /// 后者只回三个数（它内部那次查询的 `resources` 被丢掉了）。这里要明细，
    /// 就得拿查询的完整成果；两个都调会让**同一个上游接口被打两次**
    /// （批量查询时按账号翻倍）。所以调 `query_usage` 一次，再用
    /// `credits_summary_from` 摊出那份简报 —— 与改造前的响应逐字相同。
    ///
    /// ── 与网关那份的差别（迁移时改的）──────────────────────────
    /// 网关构造 `BillingService::new(AuthService::for_store(store))` 再调
    /// `query_usage`。插件这边没有那两个服务，直接用 `billing::query_usage`
    /// （自由函数，见 `billing`）—— 翻译逻辑逐字相同。
    ///
    /// 必须传**指定账号**的会话：批量查询是逐账号并发的，用「当前账号」会让
    /// 所有账号都查成同一个人。
    fn quota_sources(&self, report: &Value) -> Vec<Value> {
        crate::quota::sources(&self.meta().id, report)
    }

    fn usage<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, Value> {
        Box::pin(async move {
            let record = crate::store::record(ctx, self.region.provider_id(), account)
                .await
                .map_err(GatewayError::into_control_message)?;
            if record.is_null() {
                return Err("该账号没有可用凭证，无法查询积分".to_string());
            }
            let session = session_of_record(&record, self.region);
            let proxy = contract::proxy_of(&record).map_err(GatewayError::into_control_message)?;
            let usage = billing::query_usage(&session, proxy.as_ref(), None)
                .await
                .map_err(GatewayError::into_control_message)?;
            let mut document = billing::credits_summary_from(&usage);
            let Some(object) = document.as_object_mut() else {
                return Ok(document);
            };
            // 企业账号没有资源包（那份额度是「本周期已用 / 上限」一笔账）：
            // 明细与套餐两项都不挂，界面退回单行读数
            let packs = billing::credit_packs_from(&usage);
            if !packs.is_empty() {
                object.insert("credits".to_string(), Value::Array(packs));
            }
            if let Some(subscription) = billing::usage::subscription_of(&usage) {
                object.insert("subscription".to_string(), subscription);
            }
            Ok(document)
        })
    }

    /// 国内版每日签到（`POST /v2/billing/meter/daily-checkin`）。
    ///
    /// ── 国际版为什么报错 ────────────────────────────────────────
    /// 它没有这条接口（能力位 `checkin` 对国际版是 false，网关不会调到这里）。
    /// 真被调到就明确说清楚，而不是返回一个假的成功。
    ///
    /// ── 为什么 `Err` 只留给「请求没打成」────────────────────────
    /// 判据在 `billing::checkin::report_of`：上游的业务码分「今天已领」
    /// （→ `Done`，调度上与成功同义）、「活动未开」（→ `Inactive`）与真失败。
    /// 那些都是**结果**，靠 `message` 带到界面上；报成 `Err` 会丢掉那句文案，
    /// 用户只看到一句「签到失败」。
    fn checkin<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(async move {
            if self.region != Region::Cn {
                return Err(
                    "WorkBuddy 国际版没有普通签到（它的每日活跃在套餐领取入口）".to_string()
                );
            }
            let record = crate::store::record(ctx, self.region.provider_id(), account)
                .await
                .map_err(GatewayError::into_message)?;
            let session = session_of_record(&record, self.region);
            let claim = billing::checkin::claim(&session)
                .await
                .map_err(GatewayError::into_control_message)?;
            Ok(billing::checkin::report_of(&claim))
        })
    }

    // ── 账号面 ─────────────────────────────────────────────────

    /// 添加账号：粘贴 / 文件导入 / 官方客户端登录态导入。
    ///
    /// 三条来源都在 [`accounts::normalize`] 里收口（登录态文件的定位、嵌套形
    /// 与扁平形的取值、端点三项的落库），本方法只做区域参数的传递 ——
    /// 国内版与国际版共用一套解析，差别只有文件名与端点兜底。
    fn normalize_account<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move { accounts::normalize(input, self.region) })
    }

    fn account_view(&self, secret: &Value) -> Result<Value, String> {
        Ok(accounts::account_view(secret, self.region))
    }

    /// 稳定身份 `user-<uid>`（与内置 `add_account` 逐字一致）。
    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(accounts::identity_of(secret))
    }

    /// 管理动作（成长任务 / 猫猫旅行 / 连登管家）。
    ///
    /// 这些原来是网关的 `api::growth` 四个端点；动作名与返回形状由
    /// [`admin`] 定义，网关只透传（`admin.action` 的取值域归提供商）。
    fn admin_action<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        action: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            if matches!(action, crate::native_search::CAPABILITIES | crate::native_search::SEARCH) {
                return search::dispatch(self, ctx, account, action, payload).await;
            }
            if action == "activity.run" && self.region == Region::Intl {
                self.ensure_token(ctx, account).await?;
            }
            let mut result = admin::dispatch(ctx, self.region.provider_id(), account, action, payload).await;
            if action == "activity.run" && result.as_ref().err().is_some_and(|error| error.status_code == 401) {
                self.refresh_token(ctx, account).await?;
                result = admin::dispatch(ctx, self.region.provider_id(), account, action, payload).await;
            }
            result.map_err(|error| {
                if error.status_code == 501 {
                    agent2api_provider_sdk::unsupported(error.message)
                } else {
                    error.into_control_message()
                }
            })
        })
    }

    // ── 账号面：浏览器登录（轮询型）────────────────────────────

    /// 发起登录：问上游要 state + authUrl。
    ///
    /// `method` 目前只有一个取值（`browser` / 空串）；其余取值如实报错，
    /// 不静默按网页登录处理（静默会让「点了另一个按钮却跑了这套流程」
    /// 变成查不出来的事）。
    fn login_start<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        method: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(async move {
            if !matches!(method, "" | "browser" | "web") {
                return Err(format!(
                    "WorkBuddy 不支持登录方式「{method}」（可用：browser）"
                ));
            }
            login::login_start(self.region, payload)
                .await
                .map_err(GatewayError::into_control_message)
        })
    }

    /// 轮询一次：拿到 token 就补齐资料、返回可落库的账号。
    fn login_poll<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async move {
            login::login_poll(self.region, flow)
                .await
                .map_err(GatewayError::into_message)
        })
    }

    fn login_cancel<'a>(&'a self, _ctx: &'a PluginCtx, flow: &'a str) -> SendFuture<'a, ()> {
        Box::pin(async move {
            login::login_cancel(flow);
            Ok(())
        })
    }
}

/// 本地区默认登录态的 access token（环境变量优先，其次队首账号）。
///
/// 与改造前 `auth::get_current_session` 的优先级一致：本地区对应的环境变量
/// 存在时直接用它、完全不看账号库（脚本/CI 用户的旁路入口）。变量名按地区
/// 分开，见 `Region::env_token`。
async fn default_session_token(ctx: &PluginCtx, region: Region) -> Result<String, String> {
    if let Some(token) = region.env_token() {
        return Ok(token);
    }
    // 队首账号（`account` 为空串 = 网关给该 provider 的第一个可用账号）
    let record = crate::store::record(ctx, region.provider_id(), "")
        .await
        .map_err(|error| error.message)?;
    if record.is_null() {
        return Err("当前没有可用登录态：请先在桌面端完成登录".to_string());
    }
    let token = crate::store::pick(&record, &["accessToken", "access_token"]);
    if token.is_empty() {
        return Err("当前没有可用登录态：请先在桌面端完成登录".to_string());
    }
    Ok(token)
}
