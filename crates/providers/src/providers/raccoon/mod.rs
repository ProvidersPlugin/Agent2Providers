//! 小浣熊（raccoon）插件实现：无状态转发 + 凭证续期 + 目录 / 余额 / 签到 / 新手任务。
//!
//! ── 上游长什么样（移植来源 `raccoon-upstream-client.mjs`）────────
//!   - LLM 网关：`POST {llmBase}/chat/completions`（默认
//!     `https://xiaohuanxiong.com/api/web/llm/v2`），`Authorization: Bearer <JWT>`，
//!     `Content-Type: application/json`，OpenAI 兼容 body，SSE 流式。
//!   - 模型 id **无前缀概念**：客户端给什么就原样发给上游（`body.model` 直填）。
//!   - 鉴权：`POST {authBase}/refresh`，body `{"refresh_token": "..."}`。
//!
//! ── 与 workbuddy 的三处**关键差异**（别照抄）──────────────────
//!   1. **不注入 system 消息**：小浣熊网关是通用 OpenAI 兼容实现，源实现从不改
//!      消息序列。
//!   2. **不认「默认模型」**：本家没有这个概念（`default_models` 非空只是给界面
//!      兜底，`supports_default_model` 是网关侧的事）。
//!   3. **环境变量旁路**：`RACCOON_TOKEN` 是小浣熊自己的脚本/CI 入口 —— 插件侧
//!      照读，`anonymous_default_session = true`。
//!
//! ── 响应侧的 model 名回写 ─────────────────────────────────────
//! 上游网关会把响应 chunk 里的 `model` 换成它自己的内部名，而客户端认的是自己
//! 请求时给的名字。`sse_model_rewrite = true` 让网关的 SSE 出口做那次替换。
//!
//! ── 从网关搬来时改了什么（逐条对照，便于 parity 核对）────────────
//! ```text
//!   不变：JWT 解码、刷新单飞与比较写回、401/429 分类、SSE model 回写、
//!         目录的 10 分钟 TTL 与静态兜底、积分钱包与订阅归一、
//!         签到两步（setting_info + 账单核对）、新手任务两条一次性奖励
//!   改：  ① 凭证来源从 `store.raccoon_account_record()` 改成 `store::record()`
//!         ② 桌面端实时登录态（`~/.box-agent/config/auth.json`）**保留** ——
//!            那是本机的真实登录态，不是账号库的一部分
//!         ③ 目录缓存不落盘（网关自己缓存插件返回的清单）
//!         ④ 新手任务台账从账号记录字段改成 `ctx.ledger_put`（形状不变）
//!         ⑤ 登录换码返回 `NormalizedAccount`（落库由网关做）
//! ```
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 本 crate 编译期禁止 unwrap/expect/panic（见 `main.rs` 的 deny）。

pub mod balance;
pub mod credentials;
pub mod jwt;
pub mod models;
pub mod oauth;
pub mod onboarding;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ChatPlan, PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::{json, Value};

use crate::errors::GatewayError;

/// provider id（落盘契约，不能改）。
pub const ID: &str = "raccoon";

/// 默认 LLM 网关地址（源实现 `DEFAULT_LLM_BASE_URL`）
const DEFAULT_LLM_BASE_URL: &str = "https://xiaohuanxiong.com/api/web/llm/v2";

/// 默认鉴权 API 前缀（源实现 `DEFAULT_AUTH_ORIGIN` + `DEFAULT_AUTH_API_PREFIX`）
const DEFAULT_AUTH_ORIGIN: &str = "https://xiaohuanxiong.com";
const DEFAULT_AUTH_API_PREFIX: &str = "/api/web/auth/v1";

/// 小浣熊适配器。
pub struct RaccoonProvider;

impl Provider for RaccoonProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(ID, "小浣熊")
            .with_capabilities(proto::Capabilities {
                chat: true,
                stateful: false,
                // POST {authBase}/refresh
                refresh: true,
                // GET {llmBase}/model_catalog
                model_refresh: true,
                usage: true,
                checkin: true,
                claims: true,
                // 回调型网页登录（office-raccoon://）
                web_login: true,
                // 上游会把 chunk 里的 model 换成内部名，客户端认自己请求的那个
                sse_model_rewrite: true,
                sse_strip_newline_chunks: false,
                // RACCOON_TOKEN 是脚本/CI 的常规入口
                anonymous_default_session: true,
                system_proxy_when_unset: false,
                refresh_uses_account: true,
                // 小浣熊不认「默认模型」概念（与内置实现同一条结论）
                supports_default_model: false,
                // `RACCOON_TOKEN` 旁路实现存在
                env_credentials: true,
            })
            .with_login(vec![
                proto::LoginMethod::Browser {
                    button: "网页登录".into(),
                    hint: Some("打开小浣熊官方登录页，登录后自动回填账号".into()),
                },
                proto::LoginMethod::Token {
                    label: "登录态 JSON".into(),
                    placeholder: "粘贴 auth.json 的内容，或直接粘贴 accessToken".into(),
                    extra_fields: vec![],
                },
                proto::LoginMethod::Desktop {
                    button: "导入本机登录态".into(),
                    hint: Some("读取 ~/.box-agent/config/auth.json 里已登录的账号".into()),
                },
            ])
            .with_models(models::raw_list())
            // 账号行：`credit_detail` —— 四个钱包桶（每日积分 / 每月会员积分 /
            // 奖励积分 / 充值积分）各一行：上游只给余额一个数，没有面额 / 已用 /
            // 到期（缺的键不出），明细比读数多的只是「钱在哪个桶里」。
            // 有效期键按两种记录形态都认：插件新落的账号写 `expiresAt`，
            // 升级上来的存量账号由网关旧投影写 `tokenExpiresAt`。
            .with_account_features(proto::AccountFeatures {
                usage: true,
                credit_detail: true,
                identifier: vec!["userId".to_string(), "uid".to_string()],
                expiry: vec!["expiresAt".to_string(), "tokenExpiresAt".to_string()],
                ..Default::default()
            })
        .with_checkin(proto::CheckinInfo {
            description: Some("桌面端每日积分链路".to_string()),
            modes: vec![proto::CheckinMode::Checkin],
            onboarding: true,
            ..Default::default()
        })
    }

    fn provider_id(&self) -> String {
        ID.to_string()
    }

    // ── 数据面 ────────────────────────────────────────────────

    /// 构造 `POST {llmBase}/chat/completions`（头集合照抄源实现 `upstreamHeaders`）。
    ///
    /// body **原样透传**：源实现除了 `model` 走一次路由解析（未知模型回退默认，
    /// 本期由网关的模型校验/路由承担）之外不改任何字段。
    fn build_chat<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ChatRequest,
    ) -> SendFuture<'a, ChatPlan> {
        Box::pin(async move {
            let token = token_of(ctx, &req.account).await?;
            if token.is_empty() {
                return Err(GatewayError::with_status(
                    401,
                    "小浣熊账号缺少 accessToken，无法转发",
                )
                .into_message());
            }
            let headers: Vec<(String, String)> = vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                // 源实现给的是 `Accept: */*`（LLM 网关两种响应形态都可能回）
                ("Accept".to_string(), "*/*".to_string()),
                ("Authorization".to_string(), format!("Bearer {token}")),
            ];
            Ok(ChatPlan::chat(
                format!("{}/chat/completions", llm_base_url()),
                headers,
                req.body.clone(),
            ))
        })
    }

    /// 上游错误分类（判定依据见模块头）：
    ///   - 401 → TokenExpired
    ///   - 429 → QuotaLimited（按 HTTP 状态码；上游不给结构化恢复时间）
    ///   - 其余 → Fatal / 内容拦截
    fn classify(&self, status: u16, error_body: &Value) -> proto::ErrorClass {
        let raw = error_body
            .get("message")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误");
        let message = format!("上游返回 {status}: {raw}");
        if status == 401 {
            return proto::ErrorClass::TokenExpired { message };
        }
        if status == 429 {
            // 上游文案里没有可解析的恢复时间，reset_at 给 None 让冷却标记落兜底
            return proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: None,
                status,
            };
        }
        // 内容策略拦截（审核文案）→ ContentBlocked：不罚账号，交给编排层换中性
        // 提示词重试一次 + 触发降级
        crate::content_block::classify_or_fatal(
            status,
            error_body,
            message,
            error_body.get("code").and_then(Value::as_i64),
        )
    }

    // ── 凭证 ──────────────────────────────────────────────────

    /// 取可用 access token（临期主动刷新；刷新结果按来源回写）。
    ///
    /// - `account` 非空 → 该账号的凭证（桌面端账号实时读 auth.json）
    /// - `account` 为空 → 环境变量凭证（若配置）→ 桌面端实时登录态
    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            // 环境变量凭证优先于账号列表 —— 仅在**没有指定账号**时才看它
            // （源实现 `resolveCredentials` 的真实优先级是「账号列表 > 环境变量」，
            // 但那条链里的「账号列表」指的是**用户显式选中的账号**）。
            if account.is_empty() {
                if let Some(token) = env_access_token() {
                    return Ok(token);
                }
            }
            let credentials = credentials::snapshot(ctx, account)
                .await
                .map_err(GatewayError::into_message)?;
            let refreshed = credentials::refresh(ctx, &credentials, false)
                .await
                .map_err(GatewayError::into_message)?;
            Ok(refreshed.token)
        })
    }

    /// 401（token 被上游拒绝）后的**强制**刷新：不看临期窗口，直接续期。
    ///
    /// 为什么必须覆盖默认实现：`ensure_token` 只在「临期」时刷新，而 401
    /// 完全可能发生在一个时间上还很新的 token 上（服务端侧失效、账号被顶下线、
    /// refreshToken 轮换）。此时只调 ensure 会拿回同一个被拒的 token。
    fn refresh_token<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, String> {
        Box::pin(async move {
            if account.is_empty() && env_access_token().is_some() {
                // 环境变量凭证是静态的，没有可续期的来源
                return Err(GatewayError::with_status(
                    401,
                    "环境变量凭证（RACCOON_TOKEN）无法续期，请改用账号列表添加账号",
                )
                .into_message());
            }
            let credentials = credentials::snapshot(ctx, account)
                .await
                .map_err(GatewayError::into_message)?;
            let refreshed = credentials::refresh(ctx, &credentials, true)
                .await
                .map_err(GatewayError::into_message)?;
            Ok(refreshed.token)
        })
    }

    /// 临期判定：取该账号的凭证快照，用凭证自己的 `is_expiring()`（5 分钟窗口）
    /// 与 `can_refresh()` 判一次。
    ///
    /// 先确认记录存在：`snapshot` 在记录缺失时会回落到桌面端登录态，而
    /// 「账号已被删除」不该被回答成「需要刷新」（维护任务随后会拿这个不存在的
    /// id 去刷新）。
    fn token_expiring<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, bool> {
        Box::pin(async move {
            if account.is_empty() {
                return Ok(false);
            }
            if crate::store::record(ctx, ID, account).await.is_err() {
                return Ok(false);
            }
            match credentials::snapshot(ctx, account).await {
                Ok(credentials) => Ok(credentials.can_refresh() && credentials.is_expiring()),
                Err(_) => Ok(false),
            }
        })
    }

    // ── 目录 / 余额 / 签到 / 领取 ──────────────────────────────

    fn models<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async { Ok(models::list()) })
    }

    /// 刷新模型目录：`GET {llmBase}/model_catalog`（10 分钟缓存）。
    ///
    /// 目录刷新**不触发 token 刷新**：它只是维护动作，让一个临期 token 在这里
    /// 被续期会把日志搅乱（转发链路上该续期的地方自然会续）。因此这里只用
    /// 当前 token。没有凭证时**不带 Authorization 请求** —— 源实现同样允许
    /// 无 token 拉公开目录。
    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            let token = match credentials::snapshot(ctx, account).await {
                Ok(credentials) => credentials.token,
                Err(error) => {
                    crate::logging::verbose(
                        "[Models]",
                        &format!("小浣熊模型目录刷新：{}", error.message),
                    );
                    String::new()
                }
            };
            // 目录刷新是后台维护动作，没有请求上下文 → 跟随网关默认出口
            Ok(models::refresh(&token, None, force).await)
        })
    }

    fn usage<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, Value> {
        Box::pin(async move {
            balance::query_usage(ctx, account)
                .await
                .map_err(GatewayError::into_message)
        })
    }

    /// 每日签到：`setting_info` 触发发放 + 账单核对今日入账。
    fn checkin<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(async move {
            let claim = balance::claim_daily_grant(ctx, account)
                .await
                .map_err(GatewayError::into_message)?;
            Ok(checkin_report_of(&claim))
        })
    }

    /// 新手任务：两条一次性首次登录奖励（电脑端 / 手机端）。
    fn claim_status<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        refresh: bool,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            onboarding::get_tasks(ctx, account, refresh)
                .await
                .map_err(GatewayError::into_message)
        })
    }

    fn claim<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        _claim: &'a str,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            onboarding::claim_all(ctx, account)
                .await
                .map_err(GatewayError::into_message)
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    /// 添加账号：手填 / 文件 / 桌面端导入。
    ///
    /// ── 三种来源的语义差别（与内置版 `api::accounts` 的分支逐条对齐）──
    ///   - `Manual`：payload 里可能是嵌套的 auth.json（`access_token` 那套），
    ///     也可能是用户直接粘的 JWT。两种都认；
    ///   - `FileText`：同上，只是多一个文件名（错误文案里带上）；
    ///   - `Desktop`：**不落凭证** —— 只建一条「指向本机文件」的记录，转发时
    ///     实时读 `~/.box-agent/config/auth.json`（客户端重新登录后下次请求即生效）。
    fn normalize_account<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move { normalize(input).map_err(GatewayError::into_message) })
    }

    fn account_view(&self, secret: &Value) -> Result<Value, String> {
        Ok(account_view(secret, ""))
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(identity_of(secret))
    }

    // ── 登录 ──────────────────────────────────────────────────

    /// 授权地址（`office-raccoon://` 回调型）。
    fn login_start<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        method: &'a str,
        _payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(async move {
            if method != "browser" && !method.is_empty() {
                return Err(format!("小浣熊不支持这种登录方式：{method}"));
            }
            let state = oauth::new_login_state();
            Ok(proto::LoginStart {
                flow: state.clone(),
                url: oauth::build_authorize_url(&state),
                instructions: "在打开的页面里完成登录，成功后会自动回到应用".to_string(),
                poll_interval_ms: 0,
                external_browser: true,
                allowed_hosts: vec!["xiaohuanxiong.com".to_string()],
            })
        })
    }

    /// 用回调里的一次性 code 换凭证。
    ///
    /// ── state 校验为什么在这里再做一次 ─────────────────────────
    /// 网关的登录任务已经比对过 state（它记着发起时那一个），但插件这一侧是
    /// **真正拿 code 去换凭证**的地方 —— 多一道校验零成本，而漏掉它的后果是
    /// 「用别人的 code 换来的凭证写进你的账号库」。
    fn login_callback<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        callback: proto::LoginCallback,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async move {
            // `code` 与 `url` 两种入口给的形态不同：前者是纯 code，后者可能是
            // 整条 `office-raccoon://auth/callback?code=…&state=…`。
            let raw = if callback.url.trim().is_empty() {
                callback.code.clone()
            } else {
                callback.url.clone()
            };
            let code = if raw.contains("code=") {
                match oauth::parse_callback_code(&raw, &callback.flow) {
                    Ok(code) => code,
                    Err(error) => {
                        return Ok(proto::LoginPoll::Failed {
                            message: error.message,
                        });
                    }
                }
            } else {
                callback.code.trim().to_string()
            };
            if code.is_empty() {
                return Ok(proto::LoginPoll::Failed {
                    message: "回调里没有授权码，请重新发起登录".to_string(),
                });
            }
            match oauth::exchange_code(&code, &callback.flow).await {
                Ok(account) => Ok(proto::LoginPoll::Done { account }),
                Err(error) => Ok(proto::LoginPoll::Failed {
                    message: error.message,
                }),
            }
        })
    }
}

// ─── 公共辅助 ──────────────────────────────────────────────────

/// 账号公开视图（界面账号页读的那几个键）。
///
/// 与内置版 `raccoon_accounts` 的公开形态同一批键名：`name` / `uid` /
/// `tokenTail` / `expiresAt` / `officeIdentity`…。`tokenTail` 让用户能认出
/// 「这是哪把 token」，而完整 token 绝不下发。
pub fn account_view(secret: &Value, fallback_name: &str) -> Value {
    let token = secret
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or("");
    let user_id = secret
        .get("userId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let name = {
        let explicit = secret
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !explicit.is_empty() {
            explicit.to_string()
        } else if !fallback_name.trim().is_empty() {
            fallback_name.trim().to_string()
        } else if !user_id.is_empty() {
            format!("账号 {user_id}")
        } else {
            "小浣熊账号".to_string()
        }
    };
    let mut view = json!({
        "name": name,
        "uid": user_id,
        "userId": user_id,
        "tokenTail": crate::store::token_tail(token),
        "hasSession": true,
        "canRefresh": !secret
            .get("refreshToken")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty(),
    });
    if let Some(expires_at) = secret.get("expiresAt").and_then(Value::as_f64) {
        if let Some(object) = view.as_object_mut() {
            object.insert("expiresAt".to_string(), Value::from(expires_at));
        }
    }
    for key in ["officeIdentity", "officeOrgName", "officeOrgRole"] {
        let value = secret
            .get(key)
            .or_else(|| secret.get(&snake_case(key)))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !value.is_empty() {
            if let Some(object) = view.as_object_mut() {
                object.insert(key.to_string(), Value::String(value.to_string()));
            }
        }
    }
    view
}

/// 账号的稳定身份（去重用）：`user-<userId>`，与内置版 `add_raccoon_account`
/// 的 id 形态逐字相同 —— 同一账号重新登录会被识别成更新而不是新增。
pub fn identity_of(secret: &Value) -> String {
    let user_id = secret
        .get("userId")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if user_id.is_empty() {
        return String::new();
    }
    format!("user-{user_id}")
}

/// 驼峰 → 下划线（`officeIdentity` → `office_identity`）。
fn snake_case(key: &str) -> String {
    let mut out = String::with_capacity(key.len() + 4);
    for ch in key.chars() {
        if ch.is_ascii_uppercase() {
            out.push('_');
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// 一次请求的 token（空串 = 没有可用凭证）。
async fn token_of(ctx: &PluginCtx, account: &str) -> Result<String, String> {
    if account.is_empty() {
        if let Some(token) = env_access_token() {
            return Ok(token);
        }
    }
    match credentials::snapshot(ctx, account).await {
        Ok(credentials) => Ok(credentials.token),
        // 没有账号记录时 `snapshot` 已回落到桌面端登录态；这里只剩「真的没有」
        Err(error) => Err(error.into_message()),
    }
}

/// 环境变量里的 access token（`RACCOON_TOKEN`，空白串视为未设置）。
fn env_access_token() -> Option<String> {
    std::env::var("RACCOON_TOKEN")
        .ok()
        .map(|value| jwt::strip_bearer(&value))
        .filter(|value| !value.is_empty())
}

/// LLM 网关地址（`RACCOON_LLM_BASE_URL` 可覆盖，末尾斜杠去掉）。
pub fn llm_base_url() -> String {
    base_url_from_env("RACCOON_LLM_BASE_URL", DEFAULT_LLM_BASE_URL)
}

/// 鉴权 API 基址（env 可覆盖；默认 `{origin}{prefix}`）。
pub fn auth_api_base() -> String {
    let fallback = format!("{DEFAULT_AUTH_ORIGIN}{DEFAULT_AUTH_API_PREFIX}");
    base_url_from_env("RACCOON_AUTH_API_BASE", &fallback)
}

/// 主站 origin（`RACCOON_MAIN_SITE_URL` 可覆盖）。
///
/// ⚠️ 本家有两个消费点各自读同一个环境变量：网页登录的授权页（`oauth.rs` 的
/// `main_origin()`）与余额 / 签到（`balance.rs` 的 `main_site_url()`）。两处都
/// 是在原文件里就地实现的（搬来时逐字保留），这里**不再**提供第三份 ——
/// 三个同样的读法只会让「换域名」时漏改一处。

/// 读一个「基址」环境变量：去空白、去末尾斜杠；为空则用默认值。
///
/// 源实现还做协议校验（只允许 https 或本机 http）并在非法时**抛错**；
/// 这里不抛错（插件零 panic，且这是一个纯配置项）：非法值原样使用，
/// 请求失败时由传输层报出可读错误。
fn base_url_from_env(name: &str, fallback: &str) -> String {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| value.trim_end_matches('/').to_string())
        .unwrap_or_else(|| fallback.to_string())
}

/// 签到结果 → 协议化的 `CheckinReport`。
fn checkin_report_of(claim: &Value) -> proto::CheckinReport {
    let success = claim
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let message = claim
        .get("msg")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let credit = claim
        .get("grantsToday")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("points").and_then(Value::as_i64))
                .sum::<i64>()
        })
        .filter(|points| *points > 0)
        .map(|points| format!("{points} 积分"));
    // 「今日未见积分入账（可能已领取）」是中性结果 —— 算 Done 而不是 Failed，
    // 否则定时链会把一次正常的中性结果当成失败反复重打
    let text = message.clone().unwrap_or_default();
    let outcome = if success {
        proto::CheckinOutcome::Claimed
    } else if text.contains("已领取") || text.contains("已签到") {
        proto::CheckinOutcome::Done
    } else if text.contains("未见") || text.contains("未能核对") {
        proto::CheckinOutcome::Inactive
    } else {
        proto::CheckinOutcome::Failed
    };
    proto::CheckinReport {
        outcome,
        credit,
        streak: None,
        message,
    }
}

// ─── 账号规范化 ────────────────────────────────────────────────

/// 把三种来源统一成一条可落库的账号。
///
/// ── 为什么 Desktop 不落凭证 ────────────────────────────────
/// 桌面端账号的凭证在客户端自己的文件里，落一份快照会让「客户端重新登录」后
/// 网关还拿着旧 token。所以只建一条带 `desktop: true` 标记的记录，转发时实时读。
fn normalize(input: proto::NormalizeInput) -> Result<proto::NormalizedAccount, GatewayError> {
    match input.source {
        proto::NormalizeSource::Desktop => {
            let summary = credentials::desktop_summary()
                .map_err(|reason| GatewayError::with_status(400, reason))?;
            let user_id = summary
                .get("userId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let name = if user_id.is_empty() {
                "桌面端登录账号".to_string()
            } else {
                format!("桌面端登录账号（{user_id}）")
            };
            // 不落 token：只留身份与展示字段（`desktop: true` 让转发路径实时读文件）
            let secret = json!({
                "desktop": true,
                "userId": user_id,
                "tokenTail": summary.get("tokenTail").cloned().unwrap_or(Value::Null),
                "expiresAt": summary.get("tokenExpiresAt").cloned().unwrap_or(Value::Null),
                "name": name,
            });
            Ok(proto::NormalizedAccount {
                public: account_view(&secret, &name),
                secret,
                identity: credentials::DESKTOP_ACCOUNT_ID.to_string(),
                desktop: true,
            })
        }
        proto::NormalizeSource::Manual { payload } => from_payload(&payload, input.name.as_deref()),
        proto::NormalizeSource::FileText { content, file_name } => {
            let parsed: Value = serde_json::from_str(&content).map_err(|_| {
                let label = file_name.unwrap_or_else(|| "凭证文件".to_string());
                GatewayError::with_status(
                    400,
                    format!("{label} 不是合法 JSON（请选择小浣熊客户端登录后生成的 auth.json）"),
                )
            })?;
            from_payload(&parsed, input.name.as_deref())
        }
        proto::NormalizeSource::FileBase64 { .. } => Err(GatewayError::with_status(
            400,
            "小浣熊不支持二进制凭证文件（请选择 auth.json 这类 JSON 文件）",
        )),
    }
}

/// 从一份 JSON 载荷里解析账号（手填与文件导入共用）。
///
/// 兼容两种形态：嵌套的 auth.json（`access_token` / `refresh_token` 那套）与
/// 用户直接粘的一串 JWT。
fn from_payload(
    payload: &Value,
    explicit_name: Option<&str>,
) -> Result<proto::NormalizedAccount, GatewayError> {
    // 直接粘 JWT：整串当一个 token 处理
    let raw_token = payload.as_str().map(str::trim).unwrap_or("");
    if !raw_token.is_empty() {
        return from_token(raw_token, "", explicit_name);
    }
    let pick = |keys: &[&str]| -> String {
        for key in keys {
            let value = payload
                .get(*key)
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            if !value.is_empty() {
                return value.to_string();
            }
        }
        String::new()
    };
    let token = pick(&["token", "accessToken", "access_token", "auth_token"]);
    if token.is_empty() {
        return Err(GatewayError::with_status(
            400,
            "缺少 token（accessToken / access_token）",
        ));
    }
    let refresh_token = pick(&["refreshToken", "refresh_token"]);
    from_token(&token, &refresh_token, explicit_name).map(|mut account| {
        // office_* 三件套原样保留（续期与展示都要它们）
        if let Some(object) = account.secret.as_object_mut() {
            for (target, keys) in [
                ("officeIdentity", &["officeIdentity", "office_identity"][..]),
                ("officeOrgName", &["officeOrgName", "office_org_name"][..]),
                ("officeOrgRole", &["officeOrgRole", "office_org_role"][..]),
            ] {
                for key in keys {
                    let value = payload
                        .get(*key)
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .unwrap_or("");
                    if !value.is_empty() {
                        object.insert(target.to_string(), Value::String(value.to_string()));
                        break;
                    }
                }
            }
        }
        account.public = account_view(&account.secret, &pick(&["name", "nickname"]));
        account
    })
}

/// 从一枚 JWT 造账号（**必须**是有效 JWT：userId 与过期时刻都从它读）。
fn from_token(
    token: &str,
    refresh_token: &str,
    explicit_name: Option<&str>,
) -> Result<proto::NormalizedAccount, GatewayError> {
    let token = jwt::strip_bearer(token);
    let Some(claims) = jwt::decode_jwt_claims(&token) else {
        return Err(GatewayError::with_status(400, "token 不是有效的 JWT"));
    };
    let user_id = {
        let extracted = jwt::extract_user_id(&claims);
        if extracted.is_empty() {
            "unknown".to_string()
        } else {
            extracted
        }
    };
    let mut secret = json!({
        "accessToken": token,
        "userId": user_id,
    });
    if !refresh_token.trim().is_empty() {
        secret["refreshToken"] = Value::String(jwt::strip_bearer(refresh_token));
    }
    if let Some(expires_at) = jwt::jwt_expiry_ms(&token) {
        secret["expiresAt"] = Value::from(expires_at);
    }
    let display = jwt::display_name(&claims);
    let name = explicit_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            if display.trim().is_empty() {
                None
            } else {
                Some(display)
            }
        })
        .unwrap_or_else(|| format!("账号 {user_id}"));
    secret["name"] = Value::String(name.clone());
    Ok(proto::NormalizedAccount {
        public: account_view(&secret, &name),
        secret,
        identity: format!("user-{user_id}"),
        desktop: false,
    })
}
