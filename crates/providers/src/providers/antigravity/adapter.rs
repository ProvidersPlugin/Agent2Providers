//! Antigravity 的 `Provider` 实现：**无状态转发**（`build_chat`）。
//!
//! ── 为什么是 `build_chat` 而不是 `conversation` ──────────────
//! 上游是**无状态 HTTP**（一次 `POST …:streamGenerateContent?alt=sse` = 一次
//! 生成），只是「响应帧不是 OpenAI 方言」：SSE 每帧是 v1internal 的信封
//! （`data: {"response":{…gemini 响应…}}`，规格 §4.2），且字段路径、思考位、
//! 工具调用全按 Gemini 的形状给。插件报 `UpstreamKind::Gemini`，网关据此选
//! `AntigravityToChatStream` 翻译层 —— 于是账号轮换、限额冷却、退避重试、
//! usage 记账与取消处理全部留在网关编排层，插件只管构造与翻译。
//!
//! ── 本家有 / 没有的东西（如实声明，别照抄别家）────────────────
//!   - **有网页登录**：Google OAuth 授权码 + 插件自绑 loopback 回调
//!     （`login.rs` + `callback_server.rs`）；与粘贴 refresh token 两条入口
//!     共用 `accounts.rs` 的归一化；
//!   - **没有签到**（Antigravity 没有可自动领取的奖励活动）；
//!   - **有余额查询**（`balance.rs`）：账号级的 5 小时 / 每周额度窗口
//!     （`retrieveUserQuotaSummary` 的剩余比例）+ `loadCodeAssist` 的订阅档；
//!   - **跟随系统代理**（`system_proxy_when_unset = true`，仓内唯一）；
//!   - **不发 `x-goog-api-client`**、不采集设备指纹（见 `endpoints.rs` 的模块头）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::Value;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ChatPlan, PluginCtx, Provider, ProviderMeta, SendFuture};

use crate::content_block;
use crate::errors::GatewayError;
use crate::logging;

use super::{accounts, balance, credentials, endpoints, login, models, oauth, project, PROVIDER_ID};

/// Antigravity 提供商（无状态单例；身份全在账号记录里）。
#[derive(Clone, Copy)]
pub struct AntigravityProvider;

/// 静态单例（`providers::mod` 的注册表里放这一个）。
pub static ANTIGRAVITY: AntigravityProvider = AntigravityProvider;

impl Provider for AntigravityProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(PROVIDER_ID, "Antigravity")
            .with_capabilities(proto::Capabilities {
                chat: true,
                // 一次 HTTP = 一次生成（见模块头）
                stateful: false,
                // Google token 端点（`oauth.rs`）
                refresh: true,
                // `:fetchAvailableModels`（`models.rs`）
                model_refresh: true,
                // `:retrieveUserQuotaSummary` + `loadCodeAssist`（`balance.rs`）
                usage: true,
                // 上游没有签到活动
                checkin: false,
                claims: false,
                // Google OAuth 授权码 + loopback（`login.rs`）
                web_login: true,
                // 翻译层产出的帧带的是**上游真名**（`gemini-3.6-flash-tiered` …），
                // 客户端请求的是对外 id —— 必须回写，见 `models::upstream_model_id`
                sse_model_rewrite: true,
                sse_strip_newline_chunks: false,
                // 没有环境变量旁路
                anonymous_default_session: false,
                // ── 仓内唯一 ──────────────────────────────────────
                // 上游是 Google：多数网络里只有经代理才可达，而用户机器上
                // 「已经能打开 Google 的那个代理」就写在系统设置里。
                // **别把这一位抄到别家**（见 `super::client_for` 的论证）。
                system_proxy_when_unset: true,
                // 目录接口要账号令牌
                refresh_uses_account: true,
                // 本家是否认「默认模型」概念（协议新增位，见 `Capabilities`）
                supports_default_model: false,
                // 本家有没有环境变量 / 默认登录态旁路（协议新增位）
                env_credentials: false,
            })
            .with_login(vec![
                proto::LoginMethod::Browser {
                    button: "网页登录".to_string(),
                    hint: Some(
                        "打开 Google 授权页，完成后自动收下账号（本机访问 Google 需经代理时，\
                         请先开启系统代理）"
                            .to_string(),
                    ),
                },
                proto::LoginMethod::Token {
                    label: "登录凭据".to_string(),
                    placeholder: "粘贴 Google refresh token（以 1// 开头）".to_string(),
                    extra_fields: Vec::new(),
                },
            ])
            .with_models(models::list())
            // 账号行：`usage` —— 额度是**两个模型组 × 两个窗口**（5 小时 / 每周）
            // 的剩余比例（`retrieveUserQuotaSummary`）；上游只给比例、没有绝对数，
            // 钱包行走纯比例读数。余额列四条都画（Gemini 两条 + Claude/GPT 两条），
            // 后者正是 issue #288 要看的那份额度。
            //   `edition: false` —— 单一入口（Google OAuth 粘贴 / 网页登录）。
            //   `identifier: userId` —— 身份键存的就是 email。
            //   `expiry` 空 + `long_lived: true` —— refresh token 不过期（Google
            //     侧没有固定寿命，只在撤销授权 / 连续 6 个月不用 / 撞上同一 client
            //     的 100 个上限时才失效），access token 约 1 小时、由插件提前
            //     15 分钟自动续期。
            .with_account_features(proto::AccountFeatures {
                usage: true,
                identifier: vec!["userId".to_string()],
                long_lived: true,
                ..Default::default()
            })
    }

    fn provider_id(&self) -> String {
        PROVIDER_ID.to_string()
    }

    // ── 数据面 ────────────────────────────────────────────────

    /// 当前模型清单（内存 → 内置兜底；永远非空）。
    fn models<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async move {
            Ok(proto::ModelList {
                models: models::list(),
                meta: proto::CatalogMeta {
                    remote: models::remote_refreshed(),
                    refreshed_at: models::last_refreshed_at(),
                },
            })
        })
    }

    /// 构造 v1internal 的 `:streamGenerateContent?alt=sse` 请求。
    ///
    /// ── 为什么流式与非流式客户端都用流式端点 ─────────────────────
    /// 非流式客户端由网关把翻译后的 chat SSE 聚合出完整响应。两个端点的信封
    /// 完全一样（规格 §3.3），只维护一条 URL 判定就少一处「非流式走了另一套
    /// 信封」的分叉面；真要用 `:generateContent` 也只是换
    /// `endpoints::generate_url`。
    ///
    /// ── 基址为什么取 daily（不是 sandbox，也不是 prod）───────────
    /// 规格 §3.1：Manager 优先 sandbox、9router 的聊天流量**固定 daily**。
    /// 编排层的重试是「换账号 / 换家」而不是「换域名」，所以这里选两参考交集
    /// 里被 9router 长期使用、且不是最容易 429 的 prod 的那条 —— 端点级
    /// failover（规格 §7.15）本步未实现（`endpoints::V1_BASE_URLS` 已经有三条
    /// 基址，缺的只是让编排层换 URL 重发的那条通路）。
    ///
    /// ── 账号字段从哪读 ──────────────────────────────────────────
    /// `store::record` 把 secret 摊成记录形态，于是本家那几个键就在顶层：
    /// `accessToken`（刷新链路保证有值）、`projectId`（发现并回写）、
    /// `email`（决定信封里的 `userAgent` 标记：非 gmail/googlemail → `jetski`，
    /// Manager 的判定逐字）。`projectId` 缺失时不写信封里的 `project` 键并留
    /// 一行日志 —— 上游可能因此限制这次请求（规格 §7.16），但账号在添加 /
    /// 刷新 / 拉目录时都会尽力发现一次。
    fn build_chat<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ChatRequest,
    ) -> SendFuture<'a, ChatPlan> {
        Box::pin(async move {
            let record = crate::store::record(ctx, PROVIDER_ID, &req.account).await?;
            // 令牌：临期就刷一次（本家 access_token 只活一小时；`build_chat`
            // 是转发路径上唯一能拿到 `ctx` 的地方，刷新会回写记录）
            let credentials = oauth::ensure_fresh(ctx, &req.account, false)
                .await
                .unwrap_or_else(|_| {
                    // 刷新失败不在这里判死：手动粘贴的账号可能没有 expiresAt
                    // （`expiring()` 判不出来就不会预刷），由网关的 401 →
                    // `token.refresh` 那条链兜底。这里沿用记录里的令牌。
                    credentials::from_record(Some(&record)).unwrap_or(credentials::AntigravityCredentials {
                        id: String::new(),
                        refresh_token: String::new(),
                        access_token: String::new(),
                        expires_at: 0,
                        project_id: String::new(),
                        email: String::new(),
                        name: String::new(),
                    })
                });
            if credentials.access_token.trim().is_empty() {
                return Err(GatewayError::with_status(
                    401,
                    "Antigravity 账号缺少 access token，无法转发（请重新粘贴 refresh token，\
                     或等令牌自动刷新后再试）",
                )
                .into_message());
            }
            let requested = req
                .body
                .get("model")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            if requested.is_empty() {
                return Err(GatewayError::with_status(
                    400,
                    "Antigravity 请求缺少 model 字段（本家的模型名放在请求体里，\
                     没有路径段的模型名可用）",
                )
                .into_message());
            }
            // 对外 id → 上游真名（表见 models.rs；对不上的原样透传）
            let wire_model = models::upstream_model_id(requested);
            if wire_model != requested {
                logging::verbose(
                    "[Antigravity]",
                    &format!("按本家映射表改写模型名 {requested} → {wire_model}"),
                );
            }
            let project = credentials.project_id.trim();
            if project.is_empty() {
                logging::verbose(
                    "[Antigravity]",
                    "账号缺 cloudaicompanionProject，本次信封不带 project 字段\
                     （上游可能限制该请求；拉一次模型目录或重新登录即可发现）",
                );
            }
            let email = credentials.email.trim();
            let enterprise = !email.is_empty()
                && !email.ends_with("@gmail.com")
                && !email.ends_with("@googlemail.com");
            let envelope = super::antigravity_outbound::antigravity_request_from_chat(
                &req.body,
                &wire_model,
                project,
                enterprise,
            )
            .map_err(|message| {
                GatewayError::with_status(400, format!("Antigravity 请求转换失败：{message}"))
                    .into_message()
            })?;
            let url = endpoints::stream_generate_url(endpoints::V1_BASE_URL_DAILY);
            // content 请求的头集合（不带 `x-goog-user-project`、不带
            // `x-goog-api-client`，见 endpoints.rs 的模块头）。
            // Claude 模型另带 `anthropic-beta`（规格 §5.3：这一个头**仅 Claude**）——
            // 判据取**上游真名**（`wire_model`）：旧代 id 已被映射表重定向到 4.6，
            // 按对外的名字判会漏掉「客户端写旧名、真名是 claude-*」那一半。
            let headers = if models::is_claude_model(&wire_model) {
                endpoints::claude_content_headers(&credentials.access_token)
            } else {
                endpoints::content_headers(&credentials.access_token)
            };
            Ok(ChatPlan {
                url,
                headers,
                body: envelope,
                // Gemini v1internal 信封 + SSE：网关据此选 AntigravityToChatStream
                upstream: proto::UpstreamKind::Gemini,
            })
        })
    }

    /// 上游错误分类（规格 §7 的映射表 + 本仓的四档口径）。
    ///
    /// ── 判据为什么先看文案再看状态码 ────────────────────────────
    /// Google 把业务语义放在 `error.status` 里（`PERMISSION_DENIED` /
    /// `RESOURCE_EXHAUSTED` / `UNAVAILABLE`），HTTP 状态只是它的投影：
    ///   - `RESOURCE_EXHAUSTED` → 限额（**即使状态码不是 429** —— 规格坑 #11
    ///     记载了「官方客户端在 agent 路径撞无细节 429」这一现象，文案才是可靠信号）；
    ///   - `PERMISSION_DENIED` → 403，本家的含义是「这个账号没有资格 / 未开通」
    ///     （规格 §6：受限地区可能 403）—— 归 `TokenExpired`：编排层会先刷一次
    ///     凭证、再按队列换下一个账号，那正是这种账号该走的路；
    ///   - 其余 401 / 403 → `TokenExpired`（凭证被拒）；
    ///   - 429 → `QuotaLimited`；400 档交给共用的内容拦截判定；
    ///   - 其余 → `Fatal`（原样透出，原地退避由 [`Self::retry_advice`] 给）。
    fn classify(&self, status: u16, error_body: &Value) -> proto::ErrorClass {
        let raw = error_body
            .get("message")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误");
        let message = format!("上游返回 {status}: {raw}");
        let code = error_body.get("code").and_then(Value::as_i64);
        let lowered = raw.to_ascii_uppercase();
        if lowered.contains("RESOURCE_EXHAUSTED") {
            return proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: code,
                // 429 是下游看到的语义（额度用尽 / 限流），即使上游回的是别的码
                status: 429,
            };
        }
        if status == 401 || status == 403 {
            return proto::ErrorClass::TokenExpired { message };
        }
        if status == 429 {
            return proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: code,
                status,
            };
        }
        if status == 400 {
            return content_block::classify_or_fatal(status, error_body, message, code);
        }
        proto::ErrorClass::Fatal {
            status,
            message,
            upstream_code: code,
        }
    }

    /// 「这个错误要不要原地退避重试」。
    ///
    /// ── 与网关两条全局兜底的关系（先说清，免得误会成必需）──────
    /// 网关自己已经有两档统一兜底：`transient_retry_advice`（408 / 5xx 按状态码）
    /// 与 `fallback_retry_advice`（一切 `Fatal` 在换账号前先原地重发一次）。
    /// 因此**「5xx 可重试」这件事不靠本方法也成立** —— 本方法的增量只有两点：
    ///   1. 给出一条本家措辞的原因（会进请求日志的重试链）；
    ///   2. 覆盖「状态码不显眼、文案才是信号」的情形：Google 把瞬时故障写成
    ///      `error.status = UNAVAILABLE / INTERNAL / DEADLINE_EXCEEDED`，
    ///      message 里可能出现 `overloaded` / `try again` 这类措辞
    ///      （规格 §7.14 的首包空 / 流提前结束也归这一档）。
    ///
    /// ── 重试间隔在插件里取不到设置页的值（迁移时改的一处）──────────
    /// 网关读 `config::retry_settings().delay_ms()`（设置页「请求重试」）。
    /// 插件没有配置回调，所以用**历史硬编码**那一档：10 秒（改造前的默认，
    /// 也是设置页的出厂值）。影响面：用户把间隔改过之后，插件路径上仍按
    /// 10 秒退避 —— 比不重试好，比读配置差一点。
    fn retry_advice(
        &self,
        error_body: &Value,
        attempt: usize,
        budget: usize,
    ) -> Option<proto::RetryAdvice> {
        if attempt >= budget {
            return None;
        }
        let raw = error_body
            .get("message")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        let lowered = raw.to_ascii_lowercase();
        let transient = [
            "unavailable",
            "internal",
            "deadline_exceeded",
            "overloaded",
            "try again",
            "timeout",
        ]
        .iter()
        .any(|pattern| lowered.contains(pattern));
        if !transient {
            return None;
        }
        Some(proto::RetryAdvice {
            delay_ms: 10_000,
            reason: format!("上游暂时不可用（{raw}），稍后重试"),
        })
    }

    /// 把「映射上绑的思考等级」翻译成本家认的字段：写成 body 顶层的
    /// `reasoning_effort`，由请求转换（`antigravity_outbound` 的
    /// `thinking_budget`）映射成 `generationConfig.thinkingConfig.thinkingBudget`。
    ///
    /// 判据（三条，与 CatPaw / Qoder 的实现同一闸门）：
    ///   - 客户端已显式指定档位 → 让位（用户的明确意图比映射默认值更具体）；
    ///   - `off` / `none`：注入点已经拦下（不会问到这里）；
    ///   - 表外自定义等级（`model_rules::reasoning_rank` 返回 None）→ 不注入。
    fn reasoning_patch(&self, level: &str, _model: &str, body: &Value) -> proto::ReasoningPatch {
        if crate::model_rules::read_client_level(body)
            .filter(|value| !crate::model_rules::reasoning_is_off(value))
            .is_some()
        {
            return proto::ReasoningPatch::Skip {
                reason: "客户端请求体里已指定思考档位".to_string(),
            };
        }
        if crate::model_rules::reasoning_rank(level).is_none() {
            return proto::ReasoningPatch::Skip {
                reason: "该等级不在本家接受的档位内（本家只认通用档位）".to_string(),
            };
        }
        proto::ReasoningPatch::Set {
            field: "reasoning_effort".to_string(),
            value: Value::String(level.trim().to_string()),
        }
    }

    // ── 凭证 ──────────────────────────────────────────────────

    /// 取可用 access token：临期（提前 15 分钟）或没有 token 时刷新并回写。
    ///
    /// 刷新链（单飞 + 比较再写）在 `oauth::ensure_fresh` —— 本方法只做转调，
    /// 这样「401 后的强制刷新」与「维护任务的临期刷新」共用同一段实现。
    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let credentials = oauth::ensure_fresh(ctx, account, false).await?;
            if credentials.access_token.trim().is_empty() {
                return Err(GatewayError::with_status(
                    401,
                    "Antigravity 账号没有可用的 access token（刷新未返回令牌），请重新粘贴 refresh token",
                )
                .into_message());
            }
            Ok(credentials.access_token)
        })
    }

    /// 401 后的**强制**刷新：无视临期判定直接刷一次。
    ///
    /// 上游被拒时不会告诉我们令牌还剩多久（Google 的 401 也可能是服务端提前
    /// 失效），走临期门会原样返回刚被拒的串，让网关的「刷新后重试一次」
    /// 退化成「拿同一个坏 token 再打一次」。
    fn refresh_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let credentials = oauth::ensure_fresh(ctx, account, true).await?;
            Ok(credentials.access_token)
        })
    }

    /// 后台凭证维护问「这条账号是不是快到期了」（判据与刷新用的是同一把尺）。
    ///
    /// 没有 refresh_token 的账号返回 false：它连续期手段都没有，交给维护只会
    /// 变成每轮一条稳定失败的记录（与 Trae 的同一条处置）。
    fn token_expiring<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, bool> {
        Box::pin(async move {
            if account.trim().is_empty() {
                return Ok(false);
            }
            let record = match crate::store::record(ctx, PROVIDER_ID, account).await {
                Ok(record) if record.is_object() => record,
                _ => return Ok(false),
            };
            Ok(match credentials::from_record(Some(&record)) {
                Ok(credentials) if credentials.can_refresh() => credentials.expiring(),
                _ => false,
            })
        })
    }

    // ── 目录 ──────────────────────────────────────────────────

    /// 刷模型目录：用指定账号（空串 = 队首可用账号）的令牌打上游。
    ///
    /// 「一个账号都没有」不是失败 —— 那是「用户还没添加账号」的正常状态，
    /// 返回 `unchanged()`（与 Loomy / MonkeyCode / Command Code 同一处置：
    /// 脚本 / CI 不该看到一个红色失败）。点名的账号取不到则如实失败。
    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            let record = match crate::store::record(ctx, PROVIDER_ID, account).await {
                Ok(record) if record.is_object() => record,
                _ => {
                    if account.trim().is_empty() {
                        logging::verbose("[Models]", "Antigravity 模型目录刷新跳过：尚未添加账号");
                        return Ok(proto::RefreshOutcome::unchanged());
                    }
                    return Ok(proto::RefreshOutcome::failed(
                        "指定的账号不存在或不可用，请重新选择",
                    ));
                }
            };
            let credentials = match credentials::from_record(Some(&record)) {
                Ok(credentials) => credentials,
                Err(error) => return Ok(proto::RefreshOutcome::failed(error.message)),
            };
            let proxy = match oauth::account_proxy(&record) {
                Ok(proxy) => proxy,
                // 代理配置坏了要如实失败（不静默直连）：直连会拿到一张「从本机
                // 看得到」的表，而用户以为刷的是那条账号的出口。
                Err(error) => return Ok(proto::RefreshOutcome::failed(error.message)),
            };
            // 令牌：临期就刷一次（复用适配器那条链，成功会回写记录）
            let credentials = if credentials.expiring() {
                match oauth::ensure_fresh(ctx, account, false).await {
                    Ok(fresh) => fresh,
                    Err(error) => return Ok(proto::RefreshOutcome::failed(error.message)),
                }
            } else {
                credentials
            };
            // project：只在缺失时发现一次（best-effort，失败不影响目录）
            let mut project_id = credentials.project_id.clone();
            if project_id.trim().is_empty() {
                match project::discover_project(&credentials.access_token, proxy.as_ref()).await {
                    Ok(found) => {
                        // 发现结果回写记录（失败只记一行：记录可能已被删除）
                        let expect = crate::store::secret_view(&record);
                        let next = crate::store::with_fields(
                            &expect,
                            &[("projectId", Value::String(found.clone()))],
                        );
                        let id = record.get("id").and_then(Value::as_str).unwrap_or("");
                        if id.is_empty()
                            || !crate::store::save(ctx, id, &expect, &next).await.unwrap_or(false)
                        {
                            logging::verbose(
                                "[Antigravity]",
                                "project 发现结果未能写回账号记录（记录可能已被删除）",
                            );
                        }
                        project_id = found;
                    }
                    Err(error) => logging::verbose(
                        "[Antigravity]",
                        &format!("project 未发现（目录刷新继续）：{}", error.message),
                    ),
                }
            }
            Ok(models::refresh(&credentials.access_token, &project_id, proxy.as_ref(), force).await)
        })
    }

    // ── 余额 ──────────────────────────────────────────────────

    /// 查余额：`retrieveUserQuotaSummary` 的窗口剩余比例 + `loadCodeAssist`
    /// 的订阅档（`balance::query_usage`）。
    ///
    /// 与模型目录里的每模型 `quota` 键不是一回事：那是**目录侧**的单模型读数
    /// （随 `fetchAvailableModels` 进缓存），这里是**账号级**的窗口额度 ——
    /// 两者并存，互不替代。
    fn usage<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, Value> {
        Box::pin(async move {
            balance::query_usage(ctx, account)
                .await
                .map_err(GatewayError::into_message)
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    fn normalize_account<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(accounts::normalize(ctx, input))
    }

    fn account_view(&self, secret: &Value) -> Result<Value, String> {
        Ok(accounts::account_view(secret))
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(accounts::identity_of(secret))
    }

    // ── 登录 ──────────────────────────────────────────────────

    fn login_start<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        method: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(login::login_start(method, payload))
    }

    fn login_poll<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(login::login_poll(flow))
    }

    fn login_callback<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        callback: proto::LoginCallback,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(login::login_callback(callback))
    }

    fn login_cancel<'a>(&'a self, _ctx: &'a PluginCtx, flow: &'a str) -> SendFuture<'a, ()> {
        Box::pin(login::login_cancel(flow))
    }
}
