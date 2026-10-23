//! CatPaw 的 `Provider` 实现。
//!
//! ── 这一层解决什么问题 ──────────────────────────────────────
//! CatPaw 的协议实现（消息归一化 / 指纹链 / 会话注册表 / 轮次状态机 / 翻译层）
//! 全部收在 `providers/catpaw/` 内部，对外只留两个口子：
//!   - `conversation::run_conversation(request, sink)`；
//!   - 本文件：把 `PluginCtx` + 协议形态的 `ConversationRequest` 装配成本家的
//!     `catpaw::ConversationRequest`，再转调上面那个入口。
//! 装配以外的逻辑一行都不在这里 —— 任何「CatPaw 该怎么做」的判断都属于协议层。
//!
//! ── 与其它家的形态差异（为什么 `stateful = true`）────────────
//! WorkBuddy / 小浣熊是「一次 HTTP 请求 = 一次对话」：插件把 `ChatRequest`
//! 拼好，网关负责发。CatPaw 的一个客户端请求会变成好几个上游请求
//! （round → event → turn → 工具循环 → event），中间还有注册表与指纹链 ——
//! 这些**不能**塞进单请求契约，所以 `stateful = true`，整条发送过程在插件内。
//!
//! ── 凭证：X-Passport-Token + user-uid，且**无法刷新**───────────
//! CatPaw 的凭证是 Cookie 形态的 `X-Passport-Token` 值，`uid` 是独立请求头
//! （它不在 token 里，不是 JWT），而**没有 refreshToken 概念** —— token 过期
//! 只能在桌面端重新登录（桌面端账号的凭证是实时读 `auth.json` 的，所以
//! 「重新登录」立即恢复）。因此 `refresh` 能力位是 **false**：
//! 编排层不会为本家做「401 后刷新重试」，那条重试在这里必然拿回同一个被拒的
//! token —— 不是漏掉的分支，是正确行为。
//!
//! ── 限额 / 轮换语义的核对结论 ──────────────────────────────
//! **原项目没有多账号轮换**：`catpaw-local-proxy` 全仓没有任何 429 判定，
//! 账号选择是「用户在账号页选中的那一个」，切换账号只做一件事
//! —— 作废旧 conversation（见 `registry` 的 `clear_account`）。
//! 因此本适配器对上游错误**一律 Fatal 透传**（有状态路径连它都不会被调用：
//! `run_conversation` 内部已经把手上的 HTTP 状态与上游文案归一到 `GatewayError`）。
//!
//! ── 与网关那份的差别（迁移时改的）───────────────────────────
//! | 网关 | 插件 |
//! |---|---|
//! | `ProviderAdapter` + `AccountStore` | `Provider` + `PluginCtx` |
//! | `ForwardOutcome::Stream/Completion` | `ConversationSink`（head + chunk） |
//! | `RequestTelemetry` 抓包 / 用量 | **空实现**（见 `telemetry` 模块头） |
//! | 账号面（`core::account_store::catpaw_accounts`） | `accounts.rs` |
//! | 网页登录（`core::login::catpaw`） | `login.rs` |
//! | `env_credentials_present` / `allows_anonymous_default_session` | 协议无此位 |
//! | `outbound_reasoning` / `supports_default_model` | 协议无此钩子 |
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic；本文件不持任何锁。

use serde_json::Value;

use agent2api_provider_protocol as proto;
use agent2api_provider_protocol::WireProxy as ResolvedProxy;
use agent2api_provider_sdk::{ConversationSink, PluginCtx, Provider, ProviderMeta, SendFuture};

use crate::errors::GatewayError;
use crate::logging;
use crate::telemetry::RequestTelemetry;

use super::conversation::{run_conversation, CatPawCredentials, ConversationRequest};
use super::registry::AccountIdentity;
use super::{catalog, conversation, credentials, models, PROVIDER_ID};

/// 映射上绑的思考等级注入到请求体的哪个键。
///
/// 用 `reasoning_effort` 而不是 `effort`：`resolve_effort` 的取值链是
/// `reasoning_effort ?? reasoningEffort ?? effort`，三者都能到同一个上游字段
/// （`declarativeParams.effort`），但 `reasoning_effort` 是**最标准、也是客户端
/// 最常传的那个**（OpenAI 的 Chat 扩展字段）—— 取它能让「客户端已指定时我们
/// 跳过」与「注入」两条路在 body 上看起来是同一件事。
const REASONING_FIELD: &str = "reasoning_effort";

/// CatPaw 适配器（无状态单例：会话状态在 `conversation::registry()` 的进程级
/// 句柄里，凭证在账号存储 / auth.json / 环境变量里，本结构不持有任何字段）。
#[derive(Clone, Copy)]
pub struct CatPawProvider;

impl Provider for CatPawProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(PROVIDER_ID, "CatPaw")
            .with_capabilities(proto::Capabilities {
                chat: true,
                // 多步会话协议（见模块头）
                stateful: true,
                // 没有 refreshToken 概念（见模块头）
                refresh: false,
                // `POST /api/agent/maas/model-types`（`catalog.rs`）
                model_refresh: true,
                // `credit.catpaw.meituan.com/api/credit/balance`（`balance.rs`）
                usage: true,
                // 原项目没有签到概念
                checkin: false,
                claims: false,
                // 网页登录（passport 会话 + loopback 回调，见 `login.rs`）
                web_login: true,
                // 翻译层（`openai.rs`）自己填 `model`，通用回写层无事可做
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                // 环境变量旁路（CATPAW_COOKIE）：脚本 / CI 用户不建账号也能用
                anonymous_default_session: true,
                system_proxy_when_unset: false,
                refresh_uses_account: false,
                // 没有「默认模型」概念（与内置实现同一结论）
                supports_default_model: false,
                // `CATPAW_COOKIE` 旁路实现存在
                env_credentials: true,
            })
            .with_login(vec![
                proto::LoginMethod::Browser {
                    button: "网页登录".to_string(),
                    hint: Some("打开美团 passport 授权页，完成后自动收下账号".to_string()),
                },
                proto::LoginMethod::Token {
                    label: "登录凭据".to_string(),
                    placeholder: "粘贴 accessToken / auth.json 内容".to_string(),
                    extra_fields: vec![proto::LoginField {
                        key: "uid".to_string(),
                        label: "uid / 登录名".to_string(),
                        placeholder: "可选：账号的 uid 或登录名".to_string(),
                        optional: true,
                        multiline: false,
                    }],
                },
                proto::LoginMethod::Desktop {
                    button: "导入本机登录态".to_string(),
                    hint: Some(
                        "读取本机 CatPaw 客户端的登录态（~/.meituan-catpaw/auth.json）"
                            .to_string(),
                    ),
                },
            ])
            .with_models(catalog::list())
            // 账号行：`usage` —— `credit.catpaw.meituan.com/api/credit/balance`
            // （见 `balance.rs`）。没有 `credit_detail`：那个接口只回一个余额数，
            // 不产逐份积分包。`expiry` 空 —— 本家是会话 Cookie，没有可读的
            // 到期时刻（旧投影也不给这个键），界面显示「—」而不是编一个时间。
            .with_account_features(proto::AccountFeatures {
                usage: true,
                identifier: vec!["uid".to_string()],
                ..Default::default()
            })
    }

    /// 模型清单：**远程目录优先**，远程不可用时回落到 `MODELS` 静态表。
    ///
    /// ── 键名映射（聚合层读的那几个）────────────────────────────
    /// `id`（模型 id）+ `name`（展示名）+ `maxInputTokens` / `maxOutputTokens` /
    /// `supportsImages` / `supportsReasoning` / `supportsToolCall` / `credits`。
    /// 两条来源都由 `catalog::list()` 归一到这个形态，本方法只做转发。
    fn models<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async move {
            Ok(proto::ModelList {
                models: catalog::list(),
                meta: proto::CatalogMeta {
                    remote: !catalog::remote_models().is_empty(),
                    refreshed_at: catalog::last_refreshed_at(),
                },
            })
        })
    }

    /// 会话式转发入口：装配本家的 `ConversationRequest` 后转调协议层。
    ///
    /// ── 分工（哪些判断不属于这里）──────────────────────────────
    /// 账号选路、限额冷却、provider 轮询都在网关；轮次判定、注册表、指纹链、
    /// 图片压缩、翻译层都在协议层。本函数只做**装配**。
    fn conversation<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ConversationRequest,
        sink: ConversationSink<'a>,
    ) -> SendFuture<'a, ()> {
        Box::pin(async move {
            let mut sink = sink;
            let account_id = req.account.clone();
            let credentials = resolve_credentials(ctx, &account_id).await?;
            // ── 账号身份对账（转发选路时）───────────────────────────
            // 必须在这里做，而不是只在导入/添加账号时：桌面端实时登录态
            // （`~/.meituan-catpaw/auth.json`）由 CatPaw 客户端自己维护，用户可以在
            // 客户端里直接换一个账号登录 —— 那时 account_id（`desktop-auth` 或空）
            // 一个字都没变，账号层的作废入口完全看不出异常。这里拿**本次实际使用
            // 的凭证**里的 uid 与注册表记录的该账号身份比对，不一致就把该账号名下
            // 的会话全部作废，于是客户端换号后的**下一次请求**就会走全新会话。
            //
            // 开销：身份没变时注册表内部 O(1) 早退（一次 HashMap 查 + 一次比较）。
            let identity = AccountIdentity::new(account_id.as_str(), credentials.uid.as_str());
            conversation::registry().reconcile_identity(&identity);
            let mut request = ConversationRequest::new(
                req.body.clone(),
                ConversationRequest::session_id_from_headers(&req.headers),
                account_id.clone(),
                credentials,
                req.proxy.clone(),
                req.stream,
            );
            request.base_url = credentials::upstream_base_url();
            // 插件里的遥测是空实现（见 `telemetry` 模块头）：调用点逐字保留。
            request.telemetry = Some(std::sync::Arc::new(RequestTelemetry::new()));
            run_conversation(request, &mut sink)
                .await
                .map_err(GatewayError::into_message)
        })
    }

    /// 一律 Fatal（模块头的核对结论）：原项目没有多账号轮换，也没有可解析的
    /// 限额码 —— 上游错误在 `run_conversation` 内部已经归一到 `GatewayError`
    /// （状态码与文案都按原实现的口径映射好），本方法在正常路径上走不到。
    fn classify(&self, status: u16, error_body: &Value) -> proto::ErrorClass {
        let message = error_body
            .get("message")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误")
            .to_string();
        proto::ErrorClass::Fatal {
            status,
            message,
            upstream_code: None,
        }
    }

    /// 思考等级绑定 → `reasoning_effort`（本家是**唯一需要归并档位**的一家）。
    ///
    /// ── 为什么本家要覆写 ────────────────────────────────────────
    /// 本家的上游枚举只有 low / high / max 三个（`resolve_effort` 对别的值
    /// **当场 400**），而网关的通用候选表是 6 档。不归并就会把 `medium` /
    /// `xhigh` 这类完全合法的绑定打成 400 —— 那是把「绑定不生效」升级成
    /// 「请求失败」，方向反了。归并规则（两两合流）与理由写在
    /// `models::effort_for_level`。
    ///
    /// ── 客户端已指定时为什么不覆盖 ──────────────────────────────
    /// 判据是 `resolve_effort(body)` 读出了东西（它认 `reasoning_effort` /
    /// `reasoningEffort` / `effort` 三个键，与转发时用的是**同一个函数**）。
    /// 用户在某一次请求里显式传了档位，那是比映射上的默认值更具体的意图。
    ///
    /// **`Err` 也算「指定过」**（这一条容易写反）：客户端传了 `medium` 这类本家
    /// 不认的值时，`resolve_effort` 报错，随后 `prepare::prepare` 会给出那条
    /// 400。若此时注入绑定的档位，就把用户传错的参数**悄悄换掉**了 —— 请求
    /// 变成成功，而他永远不知道自己传的值是无效的。
    fn reasoning_patch(&self, level: &str, _model: &str, body: &Value) -> proto::ReasoningPatch {
        let declared = match models::resolve_effort(body) {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(_) => true,
        };
        if declared {
            return proto::ReasoningPatch::Skip {
                reason: "客户端请求体里已指定思考档位，绑定不覆盖".to_string(),
            };
        }
        match models::effort_for_level(level) {
            Some(effort) => proto::ReasoningPatch::Set {
                field: REASONING_FIELD.to_string(),
                value: Value::String(effort.to_string()),
            },
            None => proto::ReasoningPatch::Skip {
                reason: "该等级不在本家接受的档位内（仅 low / high / max，通用 6 档已归并）"
                    .to_string(),
            },
        }
    }

    /// 取可用凭证（`X-Passport-Token`），**含存在性校验**。
    ///
    /// `account` 为空表示「没有指定账号」：环境变量旁路（`CATPAW_COOKIE`）
    /// 优先，其次是桌面端实时登录态（`~/.meituan-catpaw/auth.json`）。
    /// 这里不返回空串假装成功 —— 拿不到就报错，且文案告诉用户该怎么补。
    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let credentials = resolve_credentials(ctx, account)
                .await
                .map_err(GatewayError::into_message)?;
            if credentials.token.trim().is_empty() {
                return Err(
                    "CatPaw 没有可用的登录凭证：请在账号页添加账号（粘贴登录态或导入桌面端登录态）"
                        .to_string(),
                );
            }
            Ok(credentials.token)
        })
    }

    /// 拉取远程模型目录（`POST /api/agent/maas/model-types`，见 `catalog.rs`）。
    ///
    /// 凭证取**当前账号**（与转发同一套 `Cookie: X-Passport-Token`）；
    /// 没有可用登录态时返回 `unchanged()`（不是错误 —— 脚本 / CI 用户走
    /// 环境变量旁路时本就不该刷目录）。
    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            // 凭证解析失败（没账号、也没桌面端登录态）→「没刷」而不是「失败」：
            // 一个不用 CatPaw 的用户点刷新时，红色失败会让他以为哪里坏了。
            let credentials = match credentials::snapshot_for(ctx, account).await {
                Ok(credentials) => credentials,
                Err(error) => {
                    logging::verbose(
                        "[Models]",
                        &format!("CatPaw 模型目录刷新跳过：{}", error.message),
                    );
                    return Ok(proto::RefreshOutcome::unchanged());
                }
            };
            let proxy = account_proxy(ctx, account).await;
            Ok(catalog::refresh(&credentials, proxy.as_ref(), force).await)
        })
    }

    /// 查余额（`catpaw/balance.rs`）。
    ///
    /// 凭证按「账号记录里的 `balanceToken` → 导入留下的 `balanceCookie.token2`」
    /// 取；都没有时返回未配置错误（前端显示成中性提示而不是红色失败）。
    ///
    /// ── 401 之后**没有**刷新重试 ──────────────────────────────
    /// `refresh` 能力位是 false，所以网关那条「401 → 刷新重试」对本家必然不做 ——
    /// 拿一个必然失败的刷新去打上游只会把一条「凭证过期」变成两条。
    fn usage<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, Value> {
        Box::pin(async move {
            super::balance::query_usage(ctx, account)
                .await
                .map_err(GatewayError::into_message)
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    fn normalize_account<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move { super::accounts::normalize(input) })
    }

    fn account_view(&self, secret: &Value) -> Result<Value, String> {
        Ok(super::accounts::account_view(secret))
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(super::accounts::identity_of(secret))
    }

    // ── 登录（回调型 / 轮询兜底）──────────────────────────────

    fn login_start<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        method: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(super::login::login_start(method, payload))
    }

    /// 轮询登录进度（本家是**双通道**：loopback 回调 + 上游 poll-token）。
    ///
    /// 报 `poll_interval_ms > 0` 的理由见 `login.rs` 模块头：回调落在**插件自己**
    /// 的 loopback 端口上，永远不经过网关的 `/api/session/login/callback`。
    fn login_poll<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(super::login::login_poll(flow))
    }

    fn login_cancel<'a>(&'a self, _ctx: &'a PluginCtx, flow: &'a str) -> SendFuture<'a, ()> {
        Box::pin(super::login::login_cancel(flow))
    }
}

/// 取本次会话转发的凭证（账号记录 / 环境变量旁路 / 桌面端实时登录态）。
///
/// 顺序：指定账号 → 该账号记录（桌面端账号实时读 auth.json）；没有指定账号 →
/// 环境变量旁路（脚本/CI）→ 桌面端实时登录态（本机的默认登录态就是它）。
/// 环境变量在**没有指定账号**时才参与：用户有账号时不该被环境变量顶掉。
pub(crate) async fn resolve_credentials(
    ctx: &PluginCtx,
    account_id: &str,
) -> Result<CatPawCredentials, GatewayError> {
    if account_id.is_empty() {
        if let Some(credentials) = credentials::env_credentials() {
            credentials::log_source("环境变量（CATPAW_COOKIE）", "", &credentials.uid);
            return Ok(credentials);
        }
    }
    let credentials = credentials::snapshot_for(ctx, account_id).await?;
    credentials::log_source(
        if account_id.is_empty() { "默认登录态" } else { "账号记录" },
        account_id,
        &credentials.uid,
    );
    Ok(credentials)
}

/// 当前账号的出网代理（目录刷新用；与转发链路同一套账号级代理）。
///
/// 账号级代理对**所有 provider 生效**，目录刷新是出网请求，因此也要走它。
/// 解析失败返回 `None`（目录刷新是只读展示动作，为一条坏代理配置报错不值得）。
async fn account_proxy(ctx: &PluginCtx, account_id: &str) -> Option<ResolvedProxy> {
    let record = crate::store::record(ctx, PROVIDER_ID, account_id)
        .await
        .ok()?;
    match crate::proxy_util::resolve_account_proxy(record.get("proxy")) {
        Some(crate::proxy_util::ProxyResolution::Resolved(proxy)) => Some(proxy),
        _ => None,
    }
}
