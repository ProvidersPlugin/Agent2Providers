//! WorkBuddy 提供商（国内版 / 国际版两家）：数据面 + 账号面。
//!
//! ── 本目录里有什么 ──────────────────────────────────────────
//! ```text
//!   mod.rs          Provider 实现（能力位、build_chat、classify、续期、余额）
//!   region.rs       两个地区（provider id / 端点 / 客户端身份 / 环境变量旁路）
//!   auth.rs         鉴权头、临期判定、续期 + 单飞
//!   login.rs        浏览器登录（轮询型：auth/state → auth/token → 补资料）
//!   catalog.rs      /v3/config 模型目录刷新
//!   builtin.rs      内置模型清单 + 准入过滤（远程拿不到时的兜底）
//!   normalize.rs    出站归一（developer→system / tool_choice / image_url / …）
//!   billing/        腾讯计费接口（积分简报、积分包明细、套餐）
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
//! | `needsRelogin` 标记 / 12153 连续计数 | **不迁**：插件只如实报错，由编排层退避（见 `refresh_account`） |
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
//! ── 成长任务域与每日活跃保活仍留在网关 ──────────────────────
//! 它们是**管理端点**（`api::growth` 的四个路由 + 签到中心的 `streak`、
//! `billing::checkin` 末尾的保活调度），不是 provider 能力位上的东西 ——
//! 协议里没有 `growth.*` 方法，硬塞进 `claim` 会让「新手任务页」的语义变成
//! 「看一眼就替用户执行一遍写操作」。那两块与插件的转发链**不共享可变状态**
//! （只读账号凭证、只打上游），所以「一半在插件、一半在网关」不会产生分叉。
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
mod catalog_state;
mod growth;
mod keepalive;
mod login;
mod normalize;
mod quota_reset;
mod region;

pub use region::Region;

// ─── 以下为 Provider 实现（原 `adapter.rs` 的正文）──────────────

use serde_json::Value;

use agent2api_provider_protocol as proto;
use agent2api_provider_protocol::WireProxy;
use agent2api_provider_sdk::{ChatPlan, PluginCtx, Provider, ProviderMeta, SendFuture};

use crate::errors::GatewayError;
use crate::ids::{new_conversation_id, new_message_id, new_span_id};
use crate::workbuddy_shared::endpoints::{normalize_endpoint, Context};

use self::quota_reset::parse_quota_reset_at;

/// 上游错误码 11-128（历史文案：Illegal API invocation from an unapproved channel）。
///
/// 实测语义：多为提示词命中上游敏感词审核而被拦截（并非单纯的频率风控）。
/// 常量名沿用 Node 版的历史命名，不要据此理解成「频率限制」。
/// 拦截会持续一小段时间，期间客户端失败自动重试会形成重试风暴并给拦截续期，
/// 因此命中 11-128 时值得退避重试 —— 次数与间隔统一走设置页的「请求重试」
/// （历史硬编码是 10 秒 / 25 秒各一次；间隔设得过短会拉长拦截窗口，建议 ≥10 秒）。
const RATE_LIMIT_CODE: i64 = 11-128;

/// 上游要求首条消息必须是 system prompt，否则返回 400
/// （first message is not system prompt）。客户端没带 system 消息时注入一条兜底系统消息。
const DEFAULT_SYSTEM_PROMPT: &str = "你是一个得力助手";

/// 上游限额码 6004（HTTP 429）：账号×模型维度的用量限额，
/// msg 形如 `您的使用量已超出频率限制，将在 2026-09-11 19:43:46 UTC+8 重置…`
/// —— 恢复时间只在文本里，由 `errors::parse_quota_reset_at` 解析。
const QUOTA_LIMIT_CODE: i64 = 6004;

/// 11-128 透传给客户端时追加的敏感词指引（避免被误当成普通频率限制）
const WAF_HINT: &str = "；提示词可能命中上游敏感词，请检查提示词";

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
        let record = crate::store::record(ctx, self.region.provider_id(), account).await?;
        Ok(session_of_record(&record, self.region))
    }

    /// 续期并写回账号（`ensure_token` 的临期分支与 `refresh_token` 共用）。
    ///
    /// ── 与网关 `AuthService::refresh_account` 的差别 ─────────────
    /// 网关那份还管三件**库侧状态**：`needsRelogin` 标记、12153 连续计数、
    /// 刷新成功后的清理。插件这边库在网关，`store_update` 只能写 secret 本体，
    /// 所以：
    ///
    ///   · **写回**：`accessToken` / `refreshToken` / `expiresAt` /
    ///     `refreshExpiresAt` 四个键（与网关 `update_account_tokens` 同名字段）；
    ///   · **不写 `needsRelogin`**：那个标记的语义是「别再刷了」，而插件没有
    ///     一个可靠的计数位置。替代做法是把 12153 如实报给编排层 —— 它会退避，
    ///     用户看到的是「请重新登录该账号」这句可执行的话；
    ///   · **`tokenTail` 不写**：那是网关给列表页显示的截断值，插件不掌握列表
    ///     形状。网关在收到写回后自己按新 token 重算（`store_update` 的网关侧
    ///     会补这一格）。
    ///
    /// ── 单飞 ────────────────────────────────────────────────────
    /// 走 [`auth::refresh_once`]：refresh token 是一次性的，两个并发刷新带同一
    /// 个 RT 去换，第二个必然 12153 而整条链就死了（issue #215）。
    ///
    /// ── 写回失败不阻断 ──────────────────────────────────────────
    /// `store_update` 返回 `false` = 另一个刷新先落盘了（值已变）。那不是错误：
    /// 我们手里这份 token 仍然可用，如实返回它即可。
    async fn refresh_account(
        &self,
        ctx: &PluginCtx,
        account: &str,
        record: &Value,
        session: &Value,
    ) -> Result<String, GatewayError> {
        let auth_value = session.get("auth").cloned().unwrap_or(Value::Null);
        let refresh_token = auth_value
            .get("refreshToken")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if refresh_token.is_empty() {
            return Err(GatewayError::with_status(
                400,
                "缺少 refreshToken，无法续期，请重新登录".to_string(),
            ));
        }
        // 按账号自己的端点 / 前缀 / 平台续期（国内版与国际版不同站点）
        let context = Context::make(
            record.get("endpoint").and_then(Value::as_str),
            record.get("prefixPath").and_then(Value::as_str),
            record.get("platform").and_then(Value::as_str),
            Some(self.region.edition_id()),
        );
        let proxy: Option<WireProxy> = record
            .get("proxy")
            .and_then(|value| serde_json::from_value(value.clone()).ok());
        let next = auth::refresh_once(
            account,
            &refresh_token,
            &auth_value,
            &context,
            proxy.as_ref(),
        )
        .await?;
        if next.access_token.is_empty() {
            return Err(GatewayError::with_status(
                502,
                "token 刷新响应缺少 accessToken".to_string(),
            ));
        }
        // ── 写回（比较再写）────────────────────────────────────
        // `expect` 必须是**读到时的那份 secret**（不是记录形态）—— 见
        // `store::secret_view` 的说明：记录形态多了 id/provider/name 三个键，
        // 直接拿去比永远不等，于是"比较再写"退化成"永远不写"。
        let expect = crate::store::secret_view(record);
        let mut next_secret = expect.clone();
        if let Some(object) = next_secret.as_object_mut() {
            object.insert(
                "accessToken".to_string(),
                Value::String(next.access_token.clone()),
            );
            if let Some(token) = next.refresh_token.as_ref().filter(|text| !text.is_empty()) {
                object.insert("refreshToken".to_string(), Value::String(token.clone()));
            }
            if let Some(value) = next.expires_at.filter(|value| *value > 0.0) {
                object.insert("expiresAt".to_string(), serde_json::json!(value));
            }
            if let Some(value) = next.refresh_expires_at.filter(|value| *value > 0.0) {
                object.insert("refreshExpiresAt".to_string(), serde_json::json!(value));
            }
        }
        match crate::store::save(ctx, account, &expect, &next_secret).await {
            Ok(true) => crate::logging::log(
                "[Accounts]",
                &format!(
                    "✅ {} 账号 token 已刷新（{account}）",
                    self.region.label()
                ),
            ),
            // 值已变 = 另一个刷新先落盘了（不是错误，见函数头）
            Ok(false) => crate::logging::verbose(
                "[Accounts]",
                &format!("账号 {account} 的凭证已被另一路刷新写过，本次不覆盖"),
            ),
            Err(error) => crate::logging::verbose(
                "[Accounts]",
                &format!("账号 {account} 的 token 写回失败：{}", error.message),
            ),
        }
        Ok(next.access_token)
    }
}

impl WorkBuddyProvider {
    /// 国内版实例
    pub const CN: Self = Self::new(Region::Cn);
    /// 国际版实例（拆家后与国内版各自独立，见模块头）
    pub const INTL: Self = Self::new(Region::Intl);
}

/// 扁平 secret → 会话形态（网关 `session_from_record` 的插件侧同义物）。
///
/// ── 折叠规则 ────────────────────────────────────────────────
/// ```text
///   account = { uid, enterpriseId, type, departmentFullName, … }   身份与组织
///   auth    = { accessToken, refreshToken, expiresAt, domain }     凭证
///   endpoint                                                        账号级落点覆盖
/// ```
/// 两边都从记录顶层取（插件 secret 是扁平的），`auth` 里那几个键与网关
/// `update_account_tokens` 写的名字逐字相同 —— 这样 `build_auth_headers` /
/// `normalize_outbound` / 计费头都能逐字复用。
pub(super) fn session_of_record(record: &Value, region: Region) -> Value {
    let text = |key: &str| -> Value {
        record
            .get(key)
            .cloned()
            .filter(|value| !value.is_null())
            .unwrap_or(Value::Null)
    };
    let mut account = serde_json::Map::new();
    for key in ["uid", "enterpriseId", "type", "departmentFullName", "name"] {
        let value = text(key);
        if !value.is_null() {
            account.insert(key.to_string(), value);
        }
    }
    let mut auth = serde_json::Map::new();
    for key in ["accessToken", "refreshToken", "expiresAt", "refreshExpiresAt", "domain"] {
        let value = text(key);
        if !value.is_null() {
            auth.insert(key.to_string(), value);
        }
    }
    let mut session = serde_json::Map::new();
    session.insert("account".to_string(), Value::Object(account));
    session.insert("auth".to_string(), Value::Object(auth));
    // 端点：账号记录上的覆盖优先；没有就由各调用方按地区兜底
    let endpoint = text("endpoint");
    if !endpoint.is_null() {
        session.insert("endpoint".to_string(), endpoint);
    }
    // 地区（`normalize` 的 `session.edition` 读它 —— 与网关同一取值域）
    session.insert(
        "edition".to_string(),
        Value::String(region.edition_id().to_string()),
    );
    // 账号 id（错误归因与日志用）
    if let Some(id) = record.get("id").and_then(Value::as_str) {
        session.insert("id".to_string(), Value::String(id.to_string()));
    }
    Value::Object(session)
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
            let session = self.session_of(ctx, &req.account).await?;
            let body = &req.body;
        // 上游只支持流式：编排层在进来之前已把 body.stream 置 true（见
        // `upstream::forward` 的说明），这里不重复设置。
        //
        // 出站归一（本家形态要求，见函数头的顺序说明）。`notable()` 为假时
        // 不写日志：常规请求唯一的改动是注入了 prompt_cache_key（几乎每次都发生），
        // 为它写一行会让详细日志被淹没；真正的修复（角色/tool 配对/图片形态等）
        // 才值得留痕 —— 那是用户排查「网关对我的请求动了什么」的唯一入口。
        let (normalized, report) = normalize::normalize_outbound(body, &session);
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

    /// 上游错误分类（照抄改造前 `upstream` 的判定与文案）：
    ///   - 401 → TokenExpired（刷新后同账号重试一次）
    ///   - 429 或 code 6004 → QuotaLimited（冷却 + 换账号）
    ///   - 内容策略拦截（11-128 或审核文案）→ ContentBlocked（**不罚账号**：换中性提示词后
    ///     同账号重试一次 + 触发降级，见 `core::degrade`）
    ///   - 其余 → Fatal（原样透传）
    ///
    /// 文案逐字对齐改造前 `rotate::request_with_waf_retry` 的那三行
    /// `format!("上游返回 {status}: {message}{hint}")`。
    fn classify(&self, status: u16, error_body: &Value) -> proto::ErrorClass {
        let code = error_body.get("code").and_then(Value::as_i64);
        let raw = error_body
            .get("message")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误");
        if status == 401 {
            return proto::ErrorClass::TokenExpired {
                message: format!("上游返回 {status}: {raw}"),
            };
        }
        if status == 429 || code == Some(QUOTA_LIMIT_CODE) {
            // 恢复时间只在上游文案里：解析失败给 None，由冷却标记落 10 分钟兜底
            let reset_at = parse_quota_reset_at(raw);
            return proto::ErrorClass::QuotaLimited {
                reset_at: if reset_at > 0 { Some(reset_at) } else { None },
                message: format!("上游返回 {status}: {raw}"),
                upstream_code: code,
                status,
            };
        }
        // 11-128 透传时追加敏感词指引（改造前在 rotate::request_with_waf_retry 里）
        let is_waf_code = code == Some(RATE_LIMIT_CODE);
        let hint = if is_waf_code { WAF_HINT } else { "" };
        let message = format!("上游返回 {status}: {raw}{hint}");
        // 内容策略拦截（11-128 及其历史文案）：**不罚账号**，交给编排层降级到
        // 中性提示词后同账号重试一次（见 `core::degrade`）。判据取并集 ——
        // 业务码兜住「上游改了文案」、共用文案规则兜住「上游改了码」，两条
        // 指向的是同一件事（论证见 `providers::content_block` 模块头）。
        //
        // 提示文案分两种：11-128 用它自己那句既有措辞（`WAF_HINT`，改造前
        // 就在 —— 既有用户的错误文案逐字不变是本项目的约定，不并进共用提示）；
        // 只按文案命中的那一支才补共用提示（两条提示说的是同一件事，
        // 叠在一起只是噪音）。
        if is_waf_code {
            return proto::ErrorClass::ContentBlocked {
                status,
                message,
                upstream_code: code,
            };
        }
        if crate::content_block::matched(status, error_body) {
            return proto::ErrorClass::ContentBlocked {
                status,
                message: format!("{message}{}", crate::content_block::CONTENT_BLOCK_HINT),
                upstream_code: code,
            };
        }
        proto::ErrorClass::Fatal {
            status,
            message,
            upstream_code: code,
        }
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
    /// 临期判 → 单飞续期 → 写回」这一条链，见 [`auth::refresh_once`]。
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
                match self.refresh_account(ctx, account, &record, &session).await {
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
            let session = session_of_record(&record, self.region);
            self.refresh_account(ctx, account, &record, &session)
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
                catalog_state::store(self.region, outcome.models.clone());
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
    fn usage<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, Value> {
        Box::pin(async move {
            let record = crate::store::record(ctx, self.region.provider_id(), account)
                .await
                .map_err(GatewayError::into_control_message)?;
            if record.is_null() {
                return Err("该账号没有可用凭证，无法查询积分".to_string());
            }
            let session = session_of_record(&record, self.region);
            let proxy = record
                .get("proxy")
                .and_then(|value| serde_json::from_value(value.clone()).ok());
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
            if let Some(subscription) = subscription_of(&usage) {
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
                .map_err(GatewayError::into_message)?;
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
            admin::dispatch(ctx, self.region.provider_id(), account, action, payload)
                .await
                .map_err(|error| {
                    if error.status_code == 501 {
                        agent2api_provider_sdk::unsupported(error.message)
                    } else {
                        error.into_message()
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
        _payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(async move {
            if !matches!(method, "" | "browser" | "web") {
                return Err(format!(
                    "WorkBuddy 不支持登录方式「{method}」（可用：browser）"
                ));
            }
            login::login_start(self.region)
                .await
                .map_err(GatewayError::into_message)
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

/// `get_personal_usage` 的成果 → 统一契约里的 `subscription`（账号页弹层头部那一行）。
///
/// ── `planName` 为什么要分档改写 ──────────────────────────────
/// 前端的套餐徽标按**上游原始名**分流（`isFreePlan` 只认 `^free$`、
/// `planBadgeLabel` 只认 `^trial$` —— ZCode 现网就给的这两个词），而本家的
/// `planName` 是积分包的**上游名**（`PackageName`，如「CodeBuddy个人体验版」；
/// 国际版给的是英文名「Bonus Pack」这类），照直传进去会让
/// **免费账号徽标走付费样式**、而且名字长到一行放不下。所以这里按
/// `editionType` 把两档免费/试用的名字归一成 ZCode 那两个词，其余（专业版 /
/// 进阶版 / 旗舰版 / 青春版）保留上游包名 —— 那几档正是用户要一眼看见的。
///
/// 企业账号（`kind: enterprise`）返回 `None`：那份读数没有套餐名，也没有
/// 会到期的包，编一个套餐名出去比空着更容易被读错。
fn subscription_of(usage: &Value) -> Option<Value> {
    if usage.get("kind").and_then(Value::as_str) == Some("enterprise") {
        return None;
    }
    let edition = usage.get("editionType").and_then(Value::as_str).unwrap_or("free");
    let plan_name = match edition {
        "free" => "Free".to_string(),
        "pro_trial" => "Trial".to_string(),
        _ => usage.get("planName").and_then(Value::as_str).unwrap_or("").trim().to_string(),
    };
    if plan_name.is_empty() {
        return None;
    }
    let mut subscription = serde_json::Map::new();
    subscription.insert("planName".to_string(), Value::String(plan_name));
    subscription.insert(
        "status".to_string(),
        Value::String(
            match edition {
                "free" => "免费",
                "pro_trial" => "试用",
                _ => "付费",
            }
            .to_string(),
        ),
    );
    // 三个可选数值：`usageLeft` / `usageTotal` 上游给的是**字符串**（Node 版
    // `String(...)`），这里原样带出去 —— 前端 `subscriptionText` 本来就是
    // `Number(...)` 读的（见 accounts-panels.tsx），两份形状不必统一成数字。
    for (from, to) in [("expireAt", "expireAt"), ("usageLeft", "remainQuota"), ("usageTotal", "totalQuota")] {
        if let Some(value) = usage.get(from) {
            if !value.is_null() {
                subscription.insert(to.to_string(), value.clone());
            }
        }
    }
    Some(Value::Object(subscription))
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

// ─── 头集合与 URL ───────────────────────────────────────────
// （最初从 upstream/request.rs 搬入；会话头族按 5.8.4 抓包升级，见 chat_headers）

/// 客户端身份 + 鉴权 + 会话追踪头。
///
/// ── 会话头族与身份对齐（2026-10 升级，渠道校验 11-128）────────
/// 国际版上游上线了「渠道校验」：对话请求缺会话头族（`X-Conversation-ID` /
/// `-Request-ID` / `-Message-ID` / `-Root-Request-ID`）时回 400 / code 11-128
/// （`conversation id missing`，displayTips「这次调用没通过渠道校验，可能不是
/// 官方客户端发出的」）。取值形态按 2026-10-16 对国际版客户端 5.8.4 的
/// mitmproxy 抓包对齐（参考实现 workbuddy2api-hub #381，两仓同步升级）：
///
/// - 会话 / 消息 id **每次请求现生成**：会话 id 是带连字符的 UUID，请求 /
///   消息 id 是 32 位无连字符 hex（UUIDv7 家族）。上游不拿这些 id 存对话
///   历史（同一 id 连发两轮、第二轮只带 system+user，模型仍答不出上一轮），
///   也不做跨请求关联，补的是「有」而不是「稳定」—— 见
///   `upstream::request::{new_conversation_id, new_message_id}`。
/// - `X-Conversation-Request-ID` 与 `X-Root-Request-ID` 是同一个值：官方在
///   一次 user send 开始时生成、该轮所有请求复用。
/// - 链路头族（`X-Trace-ID` / `traceparent` / `b3` / `X-B3-*`）照抓包补齐；
///   主请求的 `X-B3-ParentSpanId` 需要真实父子调用关系，不虚构。
/// - `X-Product` 是产品族 `SaaS`（实测值），不是产品名；`X-IDE-Name` 与
///   `X-IDE-Type` 同为 `WorkBuddy`。版本号与 UA 走 `edition.chat_ide_version`
///   / `edition.chat_ua`（国际版 5.8.4 实测值，计费 / 登录链路不受影响）。
/// - **不再发 `X-Session-ID`**：官方对话请求不带它（只用于反馈上报），此前
///   发的恒定派生值本身就是一处网关特征。
///
/// ── 客户端身份按 `region` 取（拆家后的口径）────────────────────
/// 改造前这里读的是 `session.edition`（账号上的字段）。拆家后**provider id
/// 就是权威**：账号在哪个地区组里，就按哪个地区的身份发请求 —— 这与
/// `api::session` 里 ZCode 那段注释同一条理由（拿账号字段定地区，会让
/// 「落错家的账号」把请求稳定打到错域名上，而那种错没有任何日志会提示）。
/// 账号记录里的 `endpoint` 覆盖仍然优先（见 `chat_completions_url`）。
///
/// `pub(super)`：本目录的 `keepalive`（国际版每日活跃保活）与转发共用同一套
/// 头集合 —— 保活必须长得和真对话一模一样，抄一份必然漂移。
pub(super) fn chat_headers(
    session: &Value,
    region: Region,
    accept: Option<&str>,
) -> Vec<(String, String)> {
    let edition = region.edition();
    let conversation_id = new_conversation_id();
    let message_id = new_message_id();
    let conversation_request_id = new_message_id();
    let trace_id = new_message_id();
    let span_id = new_span_id();
    // Origin / Referer 跟着会话生效端点走（staging / 自建网关账号的落点），
    // 与 `chat_completions_url` 共用同一份解析规则。
    let origin = session_endpoint(session, region);
    let authority = origin
        .split_once("://")
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_else(|| origin.clone());
    let accept_language = match region {
        Region::Intl => "en-US",
        Region::Cn => "zh-CN",
    };
    let mut headers: Vec<(String, String)> = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        // 对话链路专用 UA（国际版 5.8.4 实测；完整三段含 CLI 扩展段）
        (
            "User-Agent".to_string(),
            edition.chat_ua.to_string(),
        ),
        // 客户端身份头：服务端按此做客户端识别与白名单校验
        ("X-IDE-Type".to_string(), edition.ua_platform.to_string()),
        ("X-IDE-Name".to_string(), edition.ua_platform.to_string()),
        ("X-IDE-Version".to_string(), edition.chat_ide_version.to_string()),
        ("X-Product".to_string(), "SaaS".to_string()),
        ("X-Agent-Intent".to_string(), "craft".to_string()),
        ("X-Agent-Purpose".to_string(), "conversation".to_string()),
        ("X-Agent-Type".to_string(), "main".to_string()),
        ("X-Private-Data".to_string(), "true".to_string()),
        // 会话头族（渠道校验的判据，见函数头）
        ("X-Conversation-ID".to_string(), conversation_id),
        (
            "X-Conversation-Request-ID".to_string(),
            conversation_request_id.clone(),
        ),
        ("X-Root-Request-ID".to_string(), conversation_request_id),
        ("X-Conversation-Message-ID".to_string(), message_id.clone()),
        ("X-Request-ID".to_string(), message_id),
        // 链路头族（5.8.4 对话请求随带的观测头）
        ("X-Trace-ID".to_string(), trace_id.clone()),
        ("traceparent".to_string(), format!("00-{trace_id}-{span_id}-01")),
        ("b3".to_string(), format!("{trace_id}-{span_id}-1")),
        ("X-B3-TraceId".to_string(), trace_id),
        ("X-B3-SpanId".to_string(), span_id.clone()),
        ("X-B3-Sampled".to_string(), "1".to_string()),
        (
            "Accept-Language".to_string(),
            accept_language.to_string(),
        ),
        ("X-Requested-With".to_string(), "XMLHttpRequest".to_string()),
        ("X-CodeBuddy-Request".to_string(), "1".to_string()),
        ("Origin".to_string(), origin.clone()),
        ("Referer".to_string(), format!("{origin}/")),
    ];
    headers.extend(auth::build_auth_headers(session));
    // X-Domain 兜底：登录态没带 domain 时按端点 authority 补（与
    // `enterprise_headers` 的回退口径一致），渠道校验按它认站点。
    if !headers.iter().any(|(key, _)| key == "X-Domain") {
        headers.push(("X-Domain".to_string(), authority));
    }
    if let Some(accept) = accept {
        headers.push(("Accept".to_string(), accept.to_string()));
    }
    headers
}

/// 对话接口 URL：`{session.endpoint || 本地区端点}/v2/chat/completions`。
///
/// 兜底端点取**本实例的地区**（拆家前取 `endpoints::default_context()`，
/// 那是「本进程只有一个上游」时代的产物：它读 `WORKBUDDY_EDITION` 环境变量，
/// 而现在两个地区同时存在，环境变量不该再决定某个 provider 打哪个站）。
/// 账号记录里带 `endpoint` 时仍然以它为准 —— 那是 staging / 自建网关
/// 用户的落点，也是同一个账号级别的覆盖能力。
pub(super) fn chat_completions_url(session: &Value, region: Region) -> String {
    format!("{}/v2/chat/completions", session_endpoint(session, region))
}

/// 网页通道 agent 会话 URL：`{endpoint}/console/as/conversations/`。
///
/// 国际版上游把「当日活跃」（每日 30 积分的判定）记在网页通道的会话上
/// （issue #277：桌面端 `chat/completions` 调用成功也不计）。网页应用与桌面
/// 端同源一个站点，端点解析与 `chat_completions_url` 共用同一份规则 ——
/// 国际版默认端点本来就是 `https://www.workbuddy.ai`（网页应用的域名）。
///
/// ── 为什么在插件里没有调用方（迁移时改的）─────────────────────
/// 它唯一的消费方是网关的每日活跃保活（`keepalive::build_web_conversation_request`），
/// 而保活留在网关（见模块头「没迁的三块」）。这里保留定义只为与网关那份
/// 逐行对照 —— 哪天保活搬进来，这里就是现成的。
#[allow(dead_code)]
pub(super) fn web_conversations_url(session: &Value, region: Region) -> String {
    format!(
        "{}/console/as/conversations/",
        session_endpoint(session, region)
    )
}

/// 会话生效端点：账号记录的 `endpoint` 覆盖优先，兜底本地区的端点。
///
/// 兜底端点取**本实例的地区**（拆家前取 `endpoints::default_context()`，
/// 那是「本进程只有一个上游」时代的产物：它读 `WORKBUDDY_EDITION` 环境变量，
/// 而现在两个地区同时存在，环境变量不该再决定某个 provider 打哪个站）。
/// 账号记录里带 `endpoint` 时仍然以它为准 —— 那是 staging / 自建网关
/// 用户的落点，也是同一个账号级别的覆盖能力。
fn session_endpoint(session: &Value, region: Region) -> String {
    let fallback = region
        .env_endpoint_override()
        .unwrap_or_else(|| region.default_endpoint().to_string());
    session
        .get("endpoint")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(normalize_endpoint)
        .unwrap_or_else(|| normalize_endpoint(&fallback))
}

/// [`session_endpoint`] 的本目录内可见版本（`keepalive` 算计费端点用）。
pub(super) fn session_endpoint_of(session: &Value, region: Region) -> String {
    session_endpoint(session, region)
}

/// 首条消息不是 system 时，在头部补一条系统消息（不改原数组，返回新 body）。
///
/// 已经是 system 开头、messages 为空/缺失时返回 None（调用方保持原 body）——
/// 空 messages 属客户端异常输入，交给上游按原样报错。
pub fn ensure_leading_system_message(body: &Value) -> Option<Value> {
    let messages = body.get("messages").and_then(Value::as_array)?;
    if messages.is_empty() {
        return None;
    }
    // Node: `String(body.messages[0]?.role ?? '').toLowerCase()`
    let first_role = match messages[0].get("role") {
        Some(Value::String(text)) => text.to_lowercase(),
        Some(Value::Null) | None => String::new(),
        Some(other) => value_text(other).to_lowercase(),
    };
    if first_role == "system" {
        return None;
    }
    let mut next = body.clone();
    let Some(object) = next.as_object_mut() else {
        return None;
    };
    let mut with_system = Vec::with_capacity(messages.len() + 1);
    with_system.push(serde_json::json!({ "role": "system", "content": DEFAULT_SYSTEM_PROMPT }));
    with_system.extend(messages.iter().cloned());
    object.insert("messages".to_string(), Value::Array(with_system));
    Some(next)
}

/// JS `String(x)`（role 这类字段的容错文本化）
fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        other => other.to_string(),
    }
}
