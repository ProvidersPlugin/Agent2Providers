//! Cline 提供商（`cline-free` / `cline-pass` 两家，同一套实现按池参数化）。
//!
//! ── 上游协议（全部实测核对，2026-09）─────────────────────────
//!   - LLM：`POST {apiBase}/chat/completions`，base 是
//!     `https://api.cline.bot/api/v1`（完整路径 `/api/v1/chat/completions`）。
//!     上游用 AI SDK 的 `createOpenAICompatible`，把 baseURL 当**根**拼 ——
//!     所以路径里**只有一个 `v1`**。**发错路径的表现是 404 `{"error":"Not Found"}`**。
//!   - 认证：`Authorization: Bearer <token>`，token **含 `workos:` 前缀**
//!     （见 `credentials`）。
//!   - **产品面校验头 `X-CLIENT-TYPE: cline-sdk`（本集成的关键）**：
//!     不带这个头时**免费池模型一律 403**
//!     （`only available via Cline product surfaces`）。
//!   - **响应形态的不对称**：`stream: true` → **裸 chunk**（可直传）；
//!     非流式 → 信封 `{"data":{...},"success":true}`。网关总是以流式请求
//!     上游，所以主链路只走前者；信封只影响管理接口（`balance`）。
//!   - SSE 帧里的 `model` 是**上游内部名**（`deepseek/deepseek-v4.1-flash`），
//!     因此 `sse_model_rewrite` 必须为 true。
//!
//! ── 错误分类（实测的三种错误体）──────────────────────────────
//!   - 401 → `TokenExpired`（刷新后同账号重试一次）；
//!   - 403（`ENTITLEMENT_ERROR` / `API_REQUEST_ERROR_CODE`）→ **`Fatal`**，
//!     不是 `QuotaLimited` —— 那是「这个账号对这个模型没权限」，换账号重试
//!     同样会失败，而冷却会让账号被无意义地跳过一整个窗口；
//!   - 404 `model not found` → `Fatal`；
//!   - 429 → `QuotaLimited`（文案里带 `"Try again in 17h 59m"`，解析成
//!     `reset_at`）。
//!
//! ── 两个额度池 = 两家 provider ───────────────────────────────
//! `cline-free` 与 `cline-pass` 在上游是**同一个账号上的两个额度池**，本项目
//! 把它们当两家接入（各有独立的账号、清单、启停规则与映射）。实现是一套：
//! `ClineProvider` 持有一个 `models::Pool`，两个实例由 `providers::registry`
//! 给出。完整理由见 `models` 的模块头。
//!
//! ── 与网关那份的差别（迁移时改的）───────────────────────────
//! | 网关 | 插件 |
//! |---|---|
//! | `AccountStore` | `PluginCtx` + `store`（账号库在网关） |
//! | `catalog_cache` 持久化清单 | 网关的 `plugin_host::rpc_cache` |
//! | `model_rules::seed_cline_defaults` | 网关（模型规则是用户配置） |
//! | `config::cline_upstream_overrides()` | `ctx.settings_get` 读 `upstreamHeaders`（每次构造请求读一次快照） |
//! | `outbound_reasoning` 遥测 | **不迁**（同步钩子拿不到 body 重建后的字节） |
//! | `credentials_expiring` 的 25 分钟兜底 | **不迁**（维护任务在网关侧，它用自己的判据） |
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件绝不 unwrap/expect/panic。

mod accounts;
mod balance;
mod credentials;
mod headers;
mod login;
mod models;
mod refresh;

use std::collections::BTreeMap;

use serde_json::Value;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ChatPlan, PluginCtx, Provider, ProviderMeta, SendFuture};

use crate::content_block;
use crate::errors::GatewayError;
use crate::logging;

use models::Pool;

/// 产品面标识头（**本集成的关键**，见模块头）。
///
/// 上游按这个头判定「调用方是不是 Cline 自家产品」，缺了它免费池模型一律 403。
/// 取 `cline-sdk` 而不是 `cline-cli`：实测 `cline-cli` 会让上游走另一条
/// 兼容路径（回 500 `empty response content`），`cline-sdk` 是干净通过的那个。
///
/// `headers::DEFAULT_HEADERS` 里的 `X-CLIENT-TYPE` 一项引用本常量，不另抄
/// 字面量：改这里就同时改了发出去的客户端头。
pub const CLIENT_TYPE: &str = "cline-sdk";

/// Cline 适配器：**按池参数化**（同一套实现，两个实例）。
pub struct ClineProvider {
    /// 这个实例服务哪个池（= 哪家 provider，见 `Pool::provider_id`）
    pool: Pool,
}

impl ClineProvider {
    /// 免费池实例（provider id `cline-free`）
    pub const FREE: Self = Self::new(Pool::Free);
    /// 订阅池实例（provider id `cline-pass`）
    pub const PASS: Self = Self::new(Pool::Pass);

    /// 构造某个池的实例（`const`：两个实例都是静态量）
    pub const fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// 本实例的 provider id（`"cline-free"` / `"cline-pass"`）
    fn provider_id(&self) -> &'static str {
        self.pool.provider_id()
    }
}

impl Provider for ClineProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(
            self.pool.provider_id(),
            match self.pool {
                Pool::Free => "Cline 免费池",
                Pool::Pass => "Cline 订阅池",
            },
        )
        .with_capabilities(proto::Capabilities {
            chat: true,
            stateful: false,
            // `POST {apiBase}/auth/refresh`（见 `refresh`）
            refresh: true,
            // `GET {apiBase}/ai/cline/recommended-models`（无鉴权也能拉）
            model_refresh: true,
            usage: true,
            checkin: false,
            claims: false,
            // 设备授权登录（`login`）
            web_login: true,
            // 上游回帧的 model 是内部名（见模块头）
            sse_model_rewrite: true,
            sse_strip_newline_chunks: false,
            // 没有环境变量旁路
            anonymous_default_session: false,
            system_proxy_when_unset: false,
            // 目录接口无鉴权、清单全局：刷新不走账号维度
            refresh_uses_account: false,
            // 本家是否认「默认模型」概念（协议新增位，见 `Capabilities`）
            supports_default_model: false,
            // 本家有没有环境变量 / 默认登录态旁路（协议新增位）
            env_credentials: false,
        })
        .with_login(vec![proto::LoginMethod::Browser {
            button: "浏览器登录".to_string(),
            hint: Some(
                "会打开 Cline 的授权页：输入页面上显示的授权码即可（本机不需要回调端口）"
                    .to_string(),
            ),
        }])
        .with_models(models::list(self.pool))
        // 上游请求头的逐键覆盖编辑器：默认值与合并语义都在本模块的
        // `headers` 里，网关只存用户改过的键
        .with_admin_panels(vec![proto::AdminPanel::UpstreamHeaders])
        // 账号行：两个池是同一家上游按计费通道拆成的两个 provider，账号形态
        // 完全一样，**两边都要申报**（漏一个那一家就掉进兜底）。
        //   `identifier` 取 `uid`：公开形态里放的是 JWT 的账号 id。
        //   `expiry` 取 `expiresAt`：JWT 的 `exp` 换算成毫秒落在这里。
        //   没有 `credit_detail`：余额查询只回一个可用额度，不产逐份积分包。
        .with_account_features(proto::AccountFeatures {
            usage: true,
            identifier: vec!["uid".to_string(), "account".to_string()],
            expiry: vec!["expiresAt".to_string()],
            // 免费池的余额贴 0 / 欠费是常态，不给缺省限制规则（明确声明为空，
            // 与「没申报」区分开）；付费池沿用通用的「余额 < 1 跳过」。
            default_limiters: (self.pool == Pool::Free).then(Vec::new),
            ..Default::default()
        })
    }

    /// 本池的模型清单。
    ///
    /// 池是身份而不是账号属性，所以这里已经是收窄后的结果。
    fn models<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async move {
            Ok(proto::ModelList {
                models: models::list(self.pool),
                meta: proto::CatalogMeta {
                    remote: models::remote_refreshed(),
                    refreshed_at: models::last_refreshed_at(),
                },
            })
        })
    }

    /// 构造 `POST {apiBase}/chat/completions`。
    ///
    /// body **白名单重建**（移植自 cline-proxy 的 `buildUpstreamBody`）：
    /// 固定键注入默认值 + 白名单透传，不再是全量透传。
    fn build_chat<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ChatRequest,
    ) -> SendFuture<'a, ChatPlan> {
        Box::pin(async move {
            let record = crate::store::record(ctx, self.provider_id(), &req.account)
                .await
                .map_err(GatewayError::into_message)?;
            if record.is_null() {
                return Err("Cline 账号不存在或不属于本池（请重新添加）".to_string());
            }
            // 桌面端账号（实时登录态）：token 每次从客户端文件读，不落库
            let credentials = refresh::snapshot_for(Some(&record))
                .map_err(GatewayError::into_message)?;
            let token = credentials.bearer_token();
            if token.trim().is_empty() {
                return Err(
                    "Cline 账号缺少 accessToken，无法转发（请重新登录或导入桌面端登录态）"
                        .to_string(),
                );
            }
            // session_id 每请求生成（`sess_<毫秒>`，与官方 CLI 同形态）：进 body 的
            // `session_id` 与 `X-Task-ID` 头。
            let session_id = format!("sess_{}", logging::now_ms());
            // 客户端头覆盖表：每次构造请求读一次快照（改完下一个请求生效）。
            // 读不到就按「没有覆盖」处理 —— 全部走默认值。
            let overrides = headers_overrides(ctx, self.provider_id()).await;
            Ok(ChatPlan::chat(
                format!("{}/chat/completions", credentials::API_BASE_URL),
                build_chat_headers(&token, &session_id, overrides.as_ref()),
                build_upstream_body(&req.body, &session_id),
            ))
        })
    }

    /// 上游错误分类（判定依据全部来自实测，见模块头）。
    fn classify(&self, status: u16, error_body: &Value) -> proto::ErrorClass {
        let raw = upstream_message(error_body);
        let message = format!("上游返回 {status}: {raw}");
        if status == 401 {
            return proto::ErrorClass::TokenExpired { message };
        }
        if status == 429 {
            return proto::ErrorClass::QuotaLimited {
                // 上游不给结构化的恢复时间，但文案里带人话时长
                // （`"Try again in 17h 59m"`）—— 解析成时间戳交给编排层；
                // 解析不出维持 None（下游走 10 分钟兜底）。
                reset_at: parse_inference_cap_reset_at(error_body),
                message,
                upstream_code: None,
                status,
            };
        }
        // 内容策略拦截（审核文案）→ ContentBlocked：不罚账号，交给编排层
        // 换中性提示词重试一次 + 触发降级（见网关的 `core::degrade`）
        content_block::classify_or_fatal(status, error_body, message, None)
    }

    /// 取可用 access token：**凭证快照 + 临期主动刷新**（10 分钟窗口）。
    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let credentials = refresh::ensure_fresh(ctx, self.provider_id(), account, false)
                .await
                .map_err(GatewayError::into_message)?;
            Ok(credentials.bearer_token())
        })
    }

    /// 401（token 被上游拒绝）后的**强制**刷新：不看临期窗口，直接续期。
    ///
    /// 必须与 `ensure_token` 分开：`ensure_token` 只在临期时刷新，而 401 完全
    /// 可能发生在一个时间上还很新的 token 上（服务端侧失效、会话被顶下线）。
    fn refresh_token<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, String> {
        Box::pin(async move {
            let credentials = refresh::ensure_fresh(ctx, self.provider_id(), account, true)
                .await
                .map_err(GatewayError::into_message)?;
            Ok(credentials.bearer_token())
        })
    }

    /// 临期判定：取凭证快照，用凭证自己的 `is_expiring()` 与 `can_refresh()` 判一次。
    ///
    /// **`can_refresh()` 是必需的**：用户手填 access token 而不给 refreshToken
    /// 时，凭证天然不可刷新 —— 少这道判定会被维护任务每轮都算成「待刷新」，
    /// 然后稳定失败并往日志里灌错误。
    fn token_expiring<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, bool> {
        Box::pin(async move {
            if account.is_empty() {
                return Ok(false);
            }
            match refresh::snapshot(ctx, self.provider_id(), account).await {
                Ok(credentials) => Ok(credentials.can_refresh() && credentials.is_expiring()),
                Err(_) => Ok(false),
            }
        })
    }

    /// 拉取远程模型目录（`GET {apiBase}/ai/cline/recommended-models`）。
    ///
    /// ── 无鉴权也能拉（实测）─────────────────────────────────────
    /// 因此这里**不依赖账号状态**：一个账号都没有时也能刷新目录。
    ///
    /// ── `force` 无差别（有意）───────────────────────────────────
    /// 本家**没有 TTL 早退**：上游接口轻量且无鉴权，缓存的价值主要是
    /// 「避免每次 /v1/models 都打一次上游」，那由更上层的调用频率控制。
    fn refresh_models<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
        _force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            match models::refresh().await {
                Ok(count) => {
                    logging::log(
                        "[Models]",
                        &format!("✅ Cline 模型目录已更新（{count} 个，两个池共用）"),
                    );
                    Ok(proto::RefreshOutcome::refreshed(count))
                }
                Err(reason) => {
                    logging::verbose("[Models]", &format!("Cline 模型目录刷新失败：{reason}"));
                    Ok(proto::RefreshOutcome::failed(format!(
                        "Cline 目录刷新失败：{reason}"
                    )))
                }
            }
        })
    }

    /// 查余额 + 用量（`GET /users/{id}/balance` + `/users/me/plan`）。
    ///
    /// ── 为什么不在这里触发刷新 ──────────────────────────────────
    /// 与另外几家同一分工：适配器只提供「怎么查」，401 之后的刷新重试由
    /// 网关的 `api::accounts::query_usage_inner` 统一处置（它调
    /// `token.refresh`）。本家 `refresh: true`，因此那条链路对它是有效的。
    fn quota_sources(&self, report: &Value) -> Vec<Value> {
        crate::quota::sources(&self.meta().id, report)
    }

    fn usage<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, Value> {
        Box::pin(async move {
            balance::query_usage(ctx, self.provider_id(), account)
                .await
                .map_err(GatewayError::into_message)
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    /// 添加账号：粘贴 `accessToken` / `refreshToken`（与网关
    /// `add_cline_account` 同一套归一化）。
    ///
    /// ── 归一化做的三件事 ────────────────────────────────────────
    ///   1. 补 `workos:` 前缀（幂等）：用户手填时常从别处复制到裸 JWT，
    ///      不带前缀会稳定 401；
    ///   2. 过期时间：payload 显式给的优先，其次从 JWT 的 `exp` 推；
    ///   3. 身份 / 展示名：从 JWT 里解 email 与 `usr-…`（解不出就退到令牌摘要）。
    ///
    /// 手填与文件导入共用凭证归一；文件接受 providers.json 和单账号 JSON。
    /// 本机桌面端旁路仍不在这里启用。
    fn normalize_account<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move {
            let payload = match input.source {
                proto::NormalizeSource::Manual { payload } => payload,
                proto::NormalizeSource::Desktop => {
                    return Err(
                        "Cline 不支持导入桌面端登录态（登录态在客户端自己的目录里），\
                         请粘贴 accessToken / refreshToken"
                            .to_string(),
                    )
                }
                proto::NormalizeSource::FileText { content, .. } => {
                    return accounts::from_file(&content, input.name.as_deref());
                }
                proto::NormalizeSource::FileBase64 { .. } => {
                    return Err("Cline 不支持二进制凭证文件，请选择 providers.json 或单账号 JSON 文件".to_string())
                }
            };
            let access = crate::store::pick(&payload, &["accessToken", "access_token", "token"]);
            if access.is_empty() {
                return Err("缺少 token（accessToken / access_token）".to_string());
            }
            let refresh = crate::store::pick(&payload, &["refreshToken", "refresh_token"]);
            let access = credentials::ensure_token_prefix(&access);
            Ok(login::account_of(&access, &refresh))
        })
    }

    /// 公开视图：从 secret 现算（与 `login::account_of` 的 `public` 同形状）。
    fn account_view(&self, secret: &Value) -> Result<Value, String> {
        let access = crate::store::pick(secret, &["accessToken", "access_token", "token"]);
        let refresh = crate::store::pick(secret, &["refreshToken", "refresh_token"]);
        if access.is_empty() {
            return Err("Cline 账号缺少 accessToken".to_string());
        }
        let account = login::account_of(&access, &refresh);
        Ok(account.public)
    }

    /// 稳定身份：`usr-…` 优先，其次 JWT 里的 external_id / email，最后令牌摘要。
    ///
    /// ── 为什么不能按令牌尾巴去重 ────────────────────────────────
    /// 同一个账号刷新后令牌会变，按尾巴派生 id 会让「重新登录」变成新增一条
    /// 记录（网关那份 `add_cline_account` 的 id 取自 `account`，正是这个理由）。
    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        let access = crate::store::pick(secret, &["accessToken", "access_token", "token"]);
        if access.is_empty() {
            return Ok(String::new());
        }
        if let Some(account) = credentials::account_id_from_jwt(&access) {
            if !account.trim().is_empty() {
                return Ok(account);
            }
        }
        let (identity, _) = credentials::identity_from_jwt(&access);
        if !identity.trim().is_empty() {
            return Ok(identity);
        }
        Ok(format!("token-{}", crate::flight::fingerprint(&access)))
    }

    // ── 登录（设备授权，轮询型）────────────────────────────────

    fn login_start<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        method: &'a str,
        _payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(async move { login::login_start(method).await.map_err(GatewayError::into_message) })
    }

    fn login_poll<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async move { login::login_poll(flow).await.map_err(GatewayError::into_message) })
    }

    // ── 管理动作 ──────────────────────────────────────────────

    /// `headers.preview`：设置页「Cline 客户端头」面板要的三份表。
    ///
    /// ── 为什么这一条也要过插件 ──────────────────────────────────
    /// 默认客户端头（`X-CLIENT-TYPE: cline-sdk`、版本号、平台标识那一族）是
    /// **上游协议知识**：它们之所以长这样，是因为要贴合官方 CLI 的形态才不被
    /// 403。这类知识按项目的风险隔离要求不留在主仓库 —— 网关只保留用户那份
    /// 覆盖表（纯配置），默认值与合并语义都在插件里。
    ///
    /// 返回 `{defaults, overrides, effective}`：`overrides` 是网关存的那份，
    /// 原样回显（界面据此标出哪些行被改过）；`defaults` 让界面能区分「改回了
    /// 默认」与「本来就没改」；`effective` 是真正会发出去的那份。
    fn admin_action<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        _account: &'a str,
        action: &'a str,
        _payload: &'a Value,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            if action != "headers.preview" {
                return Err(format!("Cline 没有实现管理动作「{action}」"));
            }
            let overrides = headers_overrides(ctx, self.provider_id()).await;
            let defaults: serde_json::Map<String, Value> = headers::DEFAULT_HEADERS
                .iter()
                .map(|(key, value)| ((*key).to_string(), Value::String((*value).to_string())))
                .collect();
            let effective: serde_json::Map<String, Value> =
                headers::effective_headers(overrides.as_ref())
                    .into_iter()
                    .map(|(key, value)| (key, Value::String(value)))
                    .collect();
            let overrides_json: serde_json::Map<String, Value> = overrides
                .unwrap_or_default()
                .into_iter()
                .map(|(key, value)| (key, Value::String(value)))
                .collect();
            Ok(serde_json::json!({
                "defaults": defaults,
                "overrides": overrides_json,
                "effective": effective,
            }))
        })
    }
}

/// 读一次客户端头覆盖表（`None` = 没读到 / 网关没给）。
///
/// 读的是网关按 `plugin.<provider>.upstreamHeaders` 存的那份 JSON。
/// 失败只记 verbose：覆盖表是可选配置，读不到就全部走默认值。
async fn headers_overrides(
    ctx: &PluginCtx,
    provider: &str,
) -> Option<BTreeMap<String, String>> {
    match ctx.settings_get(provider, "upstreamHeaders").await {
        Ok(value) if !value.is_null() => Some(headers::overrides_from_json(&value)),
        Ok(_) => None,
        Err(error) => {
            logging::verbose(
                "[Cline]",
                &format!("读取客户端头覆盖表失败（按默认值发送）：{error}"),
            );
            None
        }
    }
}

/// 上游错误体 → 可读文案（本项目的归一化只保证 `message` 键，但 Cline 的
/// 两种形态是 `{"error":"..."}` 与 `{"error":{"code":..,"message":..}}`）。
///
/// `error_body` 是**已归一化**的错误对象（见 trait 契约：至少含 `code` 与
/// `message`）。归一化层取的是 `message` / `msg` / `error.message`，
/// 因此嵌套形态的 message 已经在 `message` 里了；纯字符串形态
/// （`{"error":"Unauthorized: ..."}`）取不到 `error.message`，归一化层会把
/// 整个 JSON 的截断原文放进去 —— 这里做最后一层适配：优先 `message`，
/// 空则回落到 `error` 的字符串形态。
fn upstream_message(error_body: &Value) -> String {
    if let Some(text) = error_body
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        return text.to_string();
    }
    if let Some(text) = error_body
        .get("error")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        return text.to_string();
    }
    "上游错误".to_string()
}

/// 对本池清单补种默认映射（刷新落地后调用）。
///
/// 用当前清单（含静态兜底）而不是只传远程结果：静态兜底里也有带前缀的模型，
/// 一次种完更省事，且种子本身幂等（种过的跳过）。
///
/// ── 为什么不需要「按用户所在的池排序」了（拆分后删掉的一段）────
/// 早先两池共用一个 provider，同名模型（`deepseek-v4.1-flash` 两池都有）的
/// 短名归属由**账号里有哪个池**决定 —— 只加了免费池账号的用户必须拿到
/// 免费池那条，否则短名会稳定 403（`ENTITLEMENT_ERROR`）。那套排序连同
/// 「池是账号属性」这个前提一起退场了。
///
/// 现在两家各记各的 seeded，先刷到的池拿到「去前缀」那条；两家的短名都由
/// `model_rules::EXTRA_ALIASES` 点名补挂，因此**无论哪家先刷，短名都能路由到
/// 两个池**（各有一条映射进候选链，发送名跟着实际承载的 provider 走，见
/// `catalog::wire_target_for_provider` 的 ②）。短名最终落在谁家只影响
/// 「候选链里哪个在前」，而那由账号优先级与候选链的既有规则决定，不再是
/// 一个藏在种子里、随刷新顺序漂移的隐规则。
///
/// ── 插件里没有调用方（迁移时改的）───────────────────────────
/// 网关在 `refresh_models` 成功之后自己调 `model_rules::seed_cline_defaults`
/// 种默认映射（模型规则是**用户配置**，住在网关的 `config.json` 里）。
/// 插件侧保留这个薄封装只为与网关那份逐行对照。
#[allow(dead_code)]
pub(crate) fn seed_defaults(pool: Pool) -> Option<String> {
    models::seed_cline_defaults(pool, &models::ids_of(pool))
}

// ─── 上游请求头（固定四个 + 客户端头覆盖合并）──────────────────

/// 构造上游请求头：固定四个（协议必需 + 动态任务标识）之后，客户端头
/// （默认值 + 覆盖表，见 `headers` 模块）**覆盖式**合并 ——
/// 与 cline-proxy 的 `clineHeaders` 同一顺序：后写赢，用户可以覆盖任何
/// 一个默认头，也可以新增默认清单之外的自定义头。**固定四个头在配置侧
/// 就被挡住**（网关的 `api::cline_headers` 对它们返回 400），这里的 upsert
/// 只是同一条不变量的执行侧。
fn build_chat_headers(
    token: &str,
    session_id: &str,
    overrides: Option<&BTreeMap<String, String>>,
) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "text/event-stream".to_string()),
        (
            "Authorization".to_string(),
            format!("Bearer {}", credentials::ensure_token_prefix(token)),
        ),
        // 任务标识：与 body 的 `session_id` 同值（官方 CLI 的形态）
        ("X-Task-ID".to_string(), session_id.to_string()),
    ];
    for (key, value) in headers::effective_headers(overrides) {
        upsert_header(&mut headers, key, value);
    }
    headers
}

/// 覆盖式写头：同名（HTTP 头名大小写不敏感）**替换**而非追加 ——
/// reqwest 对 `.header()` 的重复调用是追加出多值头，客户端头的覆盖语义
/// 必须是替换（与 cline-proxy `http.Header.Set` 同语义）。
fn upsert_header(headers: &mut Vec<(String, String)>, key: String, value: String) {
    if let Some(slot) = headers
        .iter_mut()
        .find(|(name, _)| name.eq_ignore_ascii_case(&key))
    {
        slot.1 = value;
    } else {
        headers.push((key, value));
    }
}

// ─── 上游请求体（白名单重建）────────────────────────────────

/// `max_tokens` 缺失时的默认值（cline-proxy 同值）。
const DEFAULT_MAX_TOKENS: i64 = 128_000;

/// `reasoning_effort` 缺失时的默认档位（cline-proxy 同值）。
const DEFAULT_REASONING_EFFORT: &str = "high";

/// 客户端**显式**给的思考档位（`reasoning_effort` → 驼峰 `reasoningEffort`，
/// 非空才算）：[`build_upstream_body`] 的默认值注入与
/// [`ClineAdapter::outbound_reasoning`] 的显示共用的**同一条取值链** ——
/// 两处各抄一份迟早分叉，而分叉的表现是「日志里说的与发出去的不是一个值」。
fn explicit_reasoning_effort(body: &Value) -> Option<&str> {
    let object = body.as_object()?;
    let text = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
    };
    text("reasoning_effort").or_else(|| text("reasoningEffort"))
}

/// 白名单透传键（照抄 cline-proxy 的 `passThroughKeys`）：客户端传了才带上。
///
/// 注意这份清单就是**全部**能上行的可选键：入站协议层（`core::protocol`）产出的
/// 顶层键里，Responses 的 `service_tier` 不在其中，会被这一层静默剔除（照抄
/// 参照实现的口径 —— 那只对 OpenAI 自己的网关有意义）。
const PASS_THROUGH_KEYS: &[&str] = &[
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "functions",
    "function_call",
    "temperature",
    "top_p",
    "top_k",
    "stop",
    "presence_penalty",
    "frequency_penalty",
    "response_format",
    "user",
    "n",
    "logit_bias",
    "seed",
    "logprobs",
    "top_logprobs",
    "stream_options",
    "metadata",
];

/// 上游请求体的**白名单重建**（移植自 cline-proxy 的 `buildUpstreamBody`）。
///
/// body 传进来时已经是「待发送的定稿」—— 模型名改写、提示词、脱敏都做完了
/// （见 `core::upstream::payload` 的分层顺序），这里按 cline-proxy 的口径把它
/// 收敛成上游认识的形状：
///   - **固定键**：`model`（**原样保留** —— 含池前缀 `cline-free/...`，那是
///     上游的通道选择器，绝不能剥，见 `models.rs`）、`max_tokens`（客户端
///     `max_tokens` → `max_completion_tokens` → 默认 128000）、`session_id`
///     （每请求生成的 `sess_<毫秒>`，与 `X-Task-ID` 头同值）、
///     `reasoning_effort`（客户端 `reasoning_effort` / 驼峰 `reasoningEffort`
///     显式值优先，否则默认 `high`）；
///   - **保留**：`messages` 与 `stream`（上游恒为流式，编排层已强制写入）；
///   - **白名单透传**：[`PASS_THROUGH_KEYS`] 里客户端传了的键原样带上，
///     白名单之外的键不上游（上游是 OpenAI 兼容网关，未知键没有意义）。
///
/// 客户端显式给了小的 `max_tokens` 时不强制抬高 —— 上游在 max_tokens 太小
/// 而模型要先输出一大段 reasoning 时会回 500 `empty response content`，
/// 那是客户端自己的取舍（见模块头「500」一节）。但 `≤ 0` 的值不算「显式小
/// 值」、按缺失处理（走 128000 默认）：参照实现会把 0 / 负数原样发出去，
/// 那对上游只会是参数错误，没有「用户本意」可谈。
fn build_upstream_body(body: &Value, session_id: &str) -> Value {
    let object = body.as_object();
    let get = |key: &str| object.and_then(|map| map.get(key));

    let mut out = serde_json::Map::new();
    if let Some(model) = get("model") {
        out.insert("model".to_string(), model.clone());
    }
    // 数值统一按 f64 读（JSON 数字在手写体里可能是 8192.0 这类浮点形态）
    let max_tokens = get("max_tokens")
        .and_then(Value::as_f64)
        .or_else(|| get("max_completion_tokens").and_then(Value::as_f64))
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| value as i64)
        .unwrap_or(DEFAULT_MAX_TOKENS);
    out.insert("max_tokens".to_string(), Value::from(max_tokens));
    out.insert("session_id".to_string(), Value::from(session_id));
    out.insert(
        "reasoning_effort".to_string(),
        Value::from(explicit_reasoning_effort(body).unwrap_or(DEFAULT_REASONING_EFFORT)),
    );
    if let Some(messages) = get("messages") {
        out.insert("messages".to_string(), messages.clone());
    }
    if let Some(stream) = get("stream") {
        out.insert("stream".to_string(), stream.clone());
    }
    for key in PASS_THROUGH_KEYS {
        if let Some(value) = get(key) {
            out.insert((*key).to_string(), value.clone());
        }
    }
    Value::Object(out)
}

// ─── 429 的人话时长解析（移植自 cline-proxy）─────────────────

/// 从 Cline 429 错误体解析 `"Try again in 17h 59m"` 形式的等待时长，
/// 返回**恢复时间戳**（毫秒；`now + 时长`）。解析不出返回 None。
///
/// 移植自 cline-proxy 的 `parseInferenceCapDuration`：在错误体序列化后的
/// JSON 全文里找 `Try again in`（等价于扫原始 body —— 归一化层不会改写
/// 文案本体），取到下一个 `"` / 换行 / `}` 为止的片段按人话时长解析。
/// 命中时编排层（`rotate::mark_account_limited`）直接采用这个时间戳、
/// 跳过 10 分钟兜底 —— Cline 免费额度撞限的真实冷却可达小时级，只按
/// 兜底冷却会反复打无效请求。
fn parse_inference_cap_reset_at(error_body: &Value) -> Option<i64> {
    let text = serde_json::to_string(error_body).ok()?;
    let duration_ms = parse_try_again_ms(&text)?;
    Some(logging::now_ms() + duration_ms)
}

/// `Try again in` 之后的片段（到下一个 `"` / 换行 / `}` 为止）→ 毫秒。
fn parse_try_again_ms(text: &str) -> Option<i64> {
    const MARKER: &str = "Try again in";
    let index = text.find(MARKER)?;
    let rest = &text[index + MARKER.len()..];
    let end = rest.find(['"', '\n', '\r', '}']).unwrap_or(rest.len());
    parse_human_duration_ms(rest[..end].trim())
}

/// 人话时长 → 毫秒。支持 `d` / `h` / `m` / `ms` / `s` 任意组合
/// （`"17h 59m"`、`"1d 2h 30m"`、`"30s"`）。
///
/// 字节扫描移植自 cline-proxy 的 `parseHumanDuration`（不用 regex，避开
/// 中文字符边界的 panic 风险，口径与 `errors::parse_quota_reset_at` 一致）：
/// 未知字符重置累积（半截 token 不算数），总时长 ≤ 0 返回 None。
fn parse_human_duration_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let mut total_ms: i64 = 0;
    let mut num: i64 = 0;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'0'..=b'9' => {
                num = num.saturating_mul(10).saturating_add((bytes[index] - b'0') as i64);
            }
            b'd' => {
                total_ms = total_ms.saturating_add(num.saturating_mul(86_400_000));
                num = 0;
            }
            b'h' => {
                total_ms = total_ms.saturating_add(num.saturating_mul(3_600_000));
                num = 0;
            }
            // "ms" 必须在裸 `m` 之前判（两个字符是一个单位）
            b'm' if bytes.get(index + 1) == Some(&b's') => {
                total_ms = total_ms.saturating_add(num);
                num = 0;
                index += 1;
            }
            b'm' => {
                total_ms = total_ms.saturating_add(num.saturating_mul(60_000));
                num = 0;
            }
            b's' => {
                total_ms = total_ms.saturating_add(num.saturating_mul(1_000));
                num = 0;
            }
            // 空格只是分隔符；其余未知字符重置累积（半截 token 不算数）
            b' ' => {}
            _ => num = 0,
        }
        index += 1;
    }
    (total_ms > 0).then_some(total_ms)
}
