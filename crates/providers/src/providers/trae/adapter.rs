//! Trae 的 `Provider` 实现。
//!
//! ── 本家在整个转发面的坐标 ──────────────────────────────────
//! Trae 走**有状态路径**（`stateful = true`）：上游 SSE 不是 OpenAI 方言
//! （没有 `[DONE]`、`token_usage` 欠到下一帧、`event: error` 之后还继续发帧），
//! 而通用无状态路径的形状是「响应字节原样透传」，中间**没有**按家翻译帧的钩子。
//! 所以发送、翻帧、错误定档都在 `forward.rs` 里做，与 Accio / Qoder 同一处置。
//! 本文件因此只剩三件对外的「协议无关」的事：分错误、取令牌与刷目录。
//!
//! ── 续期为什么在这里是真的（不是 zcode 那种如实报错）─────────
//! 本家有可用的 refreshToken 续期链（`refresh.rs`），而且它**必须**被接上：
//! Trae 的 refreshToken 每次换发都轮换，编排层「401 后刷新重试一次」这条动作
//! 如果落到一个假刷新实现上，等于拿同一串刚被服务端作废的旧串再打一次 ——
//! 不但白打，还会把「这一轮的新串」从内存里丢掉（换发结果没人接）。
//!
//! ── 与网关那份的差别（迁移时改的）───────────────────────────
//! | 网关 | 插件 |
//! |---|---|
//! | `ProviderAdapter` + `AccountStore` | `Provider` + `PluginCtx` |
//! | `ForwardOutcome::Stream/Completion` | `ConversationSink`（head + chunk） |
//! | `RequestTelemetry` 抓包 / 用量 | **空实现**（见 `telemetry` 模块头） |
//! | `store.update_trae_credentials_if_current` | `store::save`（比较再写） |
//! | 账号面（`api::accounts` 的 trae 分支） | `accounts.rs` |
//! | 登录表在网关 `core::login::trae` | `login.rs` 的进程内表 |
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic。

use serde_json::Value;

use agent2api_provider_protocol as proto;
use agent2api_provider_protocol::WireProxy as ResolvedProxy;
use agent2api_provider_sdk::{ConversationSink, PluginCtx, Provider, ProviderMeta, SendFuture};

use crate::content_block;
use crate::errors::GatewayError;
use crate::logging;

use super::credentials::Credential;
use super::errors::{classify, ErrorKind};
use super::models;
use super::refresh::{refresh_candidates, refresh_shared, MAX_ISSUE_AGE_MS, REFRESH_LEAD_MS};
use super::PROVIDER_ID;

/// Trae 提供商（无状态；身份全在账号记录里）。
#[derive(Clone, Copy)]
pub struct TraeProvider;

impl Provider for TraeProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(PROVIDER_ID, "Trae")
            .with_capabilities(proto::Capabilities {
                chat: true,
                // 上游 SSE 不是 OpenAI 方言（见模块头）
                stateful: true,
                // refreshToken 续期链（`refresh.rs`）
                refresh: true,
                // `get_detail_param` 按客户端版本给表（`models.rs`）
                model_refresh: true,
                // `ide_user_ent_usage` + `ide_user_pay_status`（`usage.rs`）
                usage: true,
                // 每日签到（`checkin.rs`）
                checkin: true,
                claims: false,
                // 网页登录（授权地址要等 guidance，见 `login.rs`）
                web_login: true,
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                refresh_uses_account: true,
                // 本家不认「默认模型」概念（客户端不指定时由上游用自己的默认）
                supports_default_model: false,
                // 没有环境变量旁路
                env_credentials: false,
            })
            .with_login(vec![
                proto::LoginMethod::Browser {
                    button: "网页登录".to_string(),
                    hint: Some("打开 Trae 授权页，完成后自动收下账号".to_string()),
                },
                proto::LoginMethod::Token {
                    label: "登录凭据".to_string(),
                    placeholder: "粘贴 CPA auth 文件内容或 accessToken".to_string(),
                    extra_fields: Vec::new(),
                },
            ])
            .with_models(models::list())
            // 账号行：`edition: false` 是事实 —— 国内 SOLO 与国际版是**两套协议**
            // 而不是一个地区的两种拼法，国际版将来接入时另立 provider id。
            //   `usage` 对应 `usage` 模块（上游两份账：`ide_user_ent_usage` 的
            //     权益包 / 积分池 + `ide_user_pay_status` 的快请求与 SOLO 并发）。
            //   `credit_detail` —— 同一份 ent_usage 里还读得到**逐份权益包**的
            //     `start_time` / `expire_time`（领取 → 到期）。签到发的包会按天
            //     堆起来，哪一份先作废只有这份清单答得上来。
            //   `credit_expiry` —— 上面那份 `expire_time` 就写成 `credits[].expireAt`
            //     （毫秒，按到期升序排好），所以「快到期优先」排序对它可用。
            .with_account_features(proto::AccountFeatures {
                usage: true,
                credit_detail: true,
                credit_expiry: true,
                identifier: vec!["uid".to_string()],
                expiry: vec!["expiresAt".to_string(), "expiresAtMs".to_string()],
                ..Default::default()
            })
        .with_checkin(proto::CheckinInfo {
            description: Some("SOLO 的 checkin_credits 领取 · 按自然日 0 点刷新".to_string()),
            modes: vec![proto::CheckinMode::Checkin],
            ..Default::default()
        })
    }

    /// 模型清单 = 上一次成功刷到的那张远程表（没刷到时为空，见 `models.rs`）。
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

    /// 有状态路径的入口（转交给 `forward.rs`；理由见模块头）。
    fn conversation<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ConversationRequest,
        sink: ConversationSink<'a>,
    ) -> SendFuture<'a, ()> {
        Box::pin(async move {
            let mut sink = sink;
            super::forward::forward(ctx, req, &mut sink)
                .await
                .map_err(GatewayError::into_message)
        })
    }

    /// 上游错误分类。
    ///
    /// 判据在 `errors::classify`（HTTP 状态 + 业务码一张表），本方法只做两件事：
    /// 把它的 `ErrorKind` 翻成编排层认识的 `ErrorClass`，以及**保住文案**。
    ///
    /// 一条本家特有的处置：`ErrInputTooLarge` / `ErrModelUnavailable` 都归到
    /// `Fatal`（不冷却账号）。大输入与「模型在这个通道不可用」是**请求级**问题 ——
    /// 同一份 body 换任何账号都会被拒，冷却健康账号只会把一次坏请求摊成
    /// 一串被罚的账号。
    fn classify(&self, status: u16, error_body: &Value) -> proto::ErrorClass {
        let detail = error_body
            .get("message")
            .or_else(|| error_body.get("msg"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误");
        let code = error_body.get("code").and_then(Value::as_i64);
        let message = format!("上游返回 {status}: {detail}");
        // 判据吃的是**整份错误体**而不是那句文案：参考实现的分类靠在 body 里
        // 找 `"code":1005` / `"code":4008` 这类片段，只传文案就会漏掉这套判据。
        match classify(status, &error_body.to_string()) {
            ErrorKind::SessionDead => proto::ErrorClass::TokenExpired { message },
            // 429 软限流与 1005/4008 计划限额都会换账号，区别只在冷却时长
            // （编排层按类别给时长，本家不提供 reset_at：上游没给可读的恢复时刻）。
            ErrorKind::SoftRate | ErrorKind::PlanLimit => proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: code,
                status,
            },
            // 只有落到「其它 4xx」这一档才交给跨家的内容拦截判定：本家的
            // 请求级分类（过大 / 通道不可用 / 404）必须优先，把它们误判成
            // 内容拦截会触发一次无意义的同账号重试。
            ErrorKind::Client => content_block::classify_or_fatal(status, error_body, message, code),
            // Server / NotFound / InputTooLarge / ModelUnavailable / None
            // 都不该罚账号。
            _ => proto::ErrorClass::Fatal {
                status,
                message,
                upstream_code: code,
            },
        }
    }

    /// 取可用令牌：临期（24h 窗口）或签发龄超 15 天时主动续期并回写。
    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move { ensure_token_inner(ctx, account).await.map_err(|error| error.message) })
    }

    /// 401 后的强制刷新：**无视临期判定直接换发**。
    ///
    /// 上游在被拒时不会告诉我们「还剩多久」，所以这条路径不能走
    /// `ensure_token` 的临期门（那会原样返回刚被拒的串，让编排层的
    /// 「刷新后重试一次」退化成「用同一个坏 token 再打一次」）。
    fn refresh_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move { refresh_token_inner(ctx, account).await.map_err(|error| error.message) })
    }

    /// 后台凭证维护问「这条账号是不是快到期了」。
    ///
    /// 漏写的症状不是报错，而是**定时维护与「立即维护」每轮都跳过本家**：
    /// 账号页长期挂着「已过期」，直到某次转发撞 401 才被懒刷新救回来 ——
    /// 而 Trae 的 refreshToken 一次一换，越晚刷越容易撞上「串已被别的程序换掉」。
    /// 判据与续期用的是同一把尺（`needs_refresh`），所以不会出现
    /// 「维护说没到期、转发时又判要刷」的分叉。
    fn token_expiring<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, bool> {
        Box::pin(async move {
            if account.trim().is_empty() {
                return Ok(false);
            }
            let record = match crate::store::record(ctx, PROVIDER_ID, account).await {
                Ok(record) if !record.is_null() => record,
                _ => return Ok(false),
            };
            Ok(match Credential::from_payload(&record) {
                // 没有 refreshToken 的账号（手工粘贴只给了 accessToken 是常态）
                // 判 false：续期手段都没有，交给维护只会变成每轮一条稳定失败。
                Ok(credential) if credential.can_refresh() => credential.needs_refresh(
                    REFRESH_LEAD_MS,
                    MAX_ISSUE_AGE_MS,
                    crate::http::now_ms(),
                ),
                _ => false,
            })
        })
    }

    /// 用**这条账号的凭据**拉一次远程目录（`get_detail_param`）。
    ///
    /// 目录是按客户端版本给的一张表（`X-Ide-Version-Code` 决定拿哪张），
    /// 不是账号无关的静态清单，所以这一家走「按账号刷」那条路
    /// （`refresh_uses_account` 保持 true）：界面上选哪条凭据刷，刷出来的
    /// 就是那条凭据看得见的表。
    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            // 「没有账号」不是失败（与 accio / qoder / codearts 同口径）：
            // 自动路径每轮都会走到这里，把它算成失败会给用户还没配置的家记上
            // 一次失败并触发冷却退避，于是新用户添加完账号、回头点「获取模型」
            // 会被「请在 N 秒后重试」挡住。
            let record = crate::store::record(ctx, PROVIDER_ID, account).await?;
            if record.is_null() {
                logging::verbose("[Models]", "Trae 模型目录刷新跳过：尚未添加 Trae 账号");
                if account.trim().is_empty() {
                    return Ok(proto::RefreshOutcome::unchanged());
                }
                return Ok(proto::RefreshOutcome::failed(
                    "指定的 Trae 账号不存在或不可用，请重新选择",
                ));
            }
            let credential = match Credential::from_payload(&record) {
                Ok(credential) => credential,
                Err(reason) => return Ok(proto::RefreshOutcome::failed(reason)),
            };
            let proxy = match account_proxy(&record) {
                Ok(proxy) => proxy,
                // 代理配置坏了要如实失败（不静默直连）：直连会拿到一张
                // 「从本机看得到」的表，而用户以为刷的是那条账号的出口。
                Err(error) => return Ok(proto::RefreshOutcome::failed(error.message)),
            };
            Ok(models::refresh(&credential, proxy.as_ref(), force).await)
        })
    }

    /// 额度读数（形状契约见 trait 文档；本家的实现细节在 `usage.rs`）。
    fn usage<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, Value> {
        Box::pin(async move {
            super::usage::query_usage(ctx, account)
                .await
                .map_err(GatewayError::into_message)
        })
    }

    fn checkin<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(async move {
            let value = super::checkin::claim_daily_checkin(ctx, account)
                .await
                .map_err(GatewayError::into_message)?;
            Ok(super::checkin_report(&value))
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    fn normalize_account<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(super::accounts::normalize(input))
    }

    fn account_view(&self, secret: &Value) -> Result<Value, String> {
        Ok(super::accounts::account_view(secret))
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(super::accounts::identity_of(secret))
    }

    // ── 登录（回调型）────────────────────────────────────────

    fn login_start<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        method: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(super::login::login_start(method, payload))
    }

    /// 轮询登录进度。
    ///
    /// ── 本家明明是回调型，为什么还要实现 poll ───────────────────
    /// 回调打在**插件自己**的 loopback 端口上，永远不经过网关的
    /// `/api/session/login/callback` —— 所以 `login_start` 报的是
    /// `poll_interval_ms > 0`（「轮询型」在协议里的含义是「网关主动来问」，
    /// 与授权在哪里终结无关）。网关因此只会调 `login.poll`，**不会**调
    /// `login.callback`（那是内置家的回调入口）。
    fn login_poll<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(super::login::login_poll(flow))
    }

    /// 用户把回调 URL 粘回来时的入口（远程浏览器 / 容器场景）。
    ///
    /// 走与真实 loopback 回调同一条换证链：投递给监听器，再由后台任务照常
    /// 换证 —— 不复制第二套逻辑。
    fn login_callback<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        callback: proto::LoginCallback,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(super::login::login_callback(callback))
    }

    fn login_cancel<'a>(&'a self, _ctx: &'a PluginCtx, flow: &'a str) -> SendFuture<'a, ()> {
        Box::pin(super::login::login_cancel(flow))
    }
}

/// `ensure_token` 的内部实现（保留 `GatewayError` 以便区分状态码）。
async fn ensure_token_inner(ctx: &PluginCtx, account: &str) -> Result<String, GatewayError> {
    let record = read_record(ctx, account).await?;
    let mut credential = Credential::from_payload(&record).map_err(GatewayError::new)?;
    if credential.needs_refresh(REFRESH_LEAD_MS, MAX_ISSUE_AGE_MS, crate::http::now_ms()) {
        let proxy = account_proxy(&record)?;
        credential = renew(ctx, &record, &credential, proxy.as_ref()).await?;
    }
    if credential.access_token.trim().is_empty() {
        return Err(GatewayError::with_status(
            401,
            "Trae 账号缺少 accessToken，请重新登录",
        ));
    }
    Ok(credential.access_token)
}

/// `refresh_token` 的内部实现（无条件换发）。
async fn refresh_token_inner(ctx: &PluginCtx, account: &str) -> Result<String, GatewayError> {
    let record = read_record(ctx, account).await?;
    let credential = Credential::from_payload(&record).map_err(GatewayError::new)?;
    let proxy = account_proxy(&record)?;
    let renewed = renew(ctx, &record, &credential, proxy.as_ref()).await?;
    Ok(renewed.access_token)
}

/// 续期 + 回写（两条取令牌路径共用这一段，分叉的代价是「一处比较一处不比」）。
/// 临期（或签发龄超上限）才换发；否则原样返回。
pub(crate) async fn renew_if_due(
    ctx: &PluginCtx,
    record: &Value,
    credential: &Credential,
    proxy: Option<&ResolvedProxy>,
) -> Result<Credential, GatewayError> {
    if credential.needs_refresh(REFRESH_LEAD_MS, MAX_ISSUE_AGE_MS, crate::http::now_ms()) {
        return renew(ctx, record, credential, proxy).await;
    }
    Ok(credential.clone())
}

/// 无视临期判定直接换发（401 / 1001 之后救一次用的就是这条）。
pub(crate) async fn renew_forced(
    ctx: &PluginCtx,
    record: &Value,
    credential: &Credential,
    proxy: Option<&ResolvedProxy>,
) -> Result<Credential, GatewayError> {
    renew(ctx, record, credential, proxy).await
}

/// 换发并回写。
///
/// ── 写回为什么是「比较再写」─────────────────────────────────
/// 网关那份用 `update_trae_credentials_if_current`：逐键比对 5 个字段后整份
/// 写入。插件侧的 `store::save` 是同一语义的通用形态（`expect` 是读到时的
/// 那份 secret，网关只在它没变时才写）。
///
/// ── 期望值已变时为什么报 409 而不是静默重读 ─────────────────
/// 记录在续期期间被别人改过（并发登录 / 手工粘贴）时，**不能**把这次换发
/// 结果写进去，也不能把新串丢掉不管 —— 报一句让用户知道 refreshToken
/// 可能已经被服务端轮换过，下一次重试会以落盘那份为准。
async fn renew(
    ctx: &PluginCtx,
    record: &Value,
    credential: &Credential,
    proxy: Option<&ResolvedProxy>,
) -> Result<Credential, GatewayError> {
    if !credential.can_refresh() {
        return Err(GatewayError::with_status(
            401,
            "Trae 账号里没有 refreshToken，无法续期（请在「账号」页重新登录）",
        ));
    }
    let renewed =
        refresh_shared(credential, &refresh_candidates(&credential.api_host), proxy).await?;
    let expect = crate::store::secret_view(record);
    let next = merge_credential(&expect, &renewed);
    let account_id = record.get("id").and_then(Value::as_str).unwrap_or("");
    if account_id.is_empty() {
        return Err(GatewayError::with_status(
            409,
            "Trae 续期结果无法写回（账号记录缺 id），请重新添加该账号",
        ));
    }
    if !crate::store::save(ctx, account_id, &expect, &next).await? {
        return Err(GatewayError::with_status(
            409,
            "Trae 续期结果与账号记录不匹配（期间被改过），本次换发未落盘，请重试",
        ));
    }
    Ok(renewed)
}

/// 把换发结果并进既有 secret（空值不覆盖）。
///
/// 与网关 `update_trae_credentials_if_current` 的「只并非空字段」逐条对齐：
/// 手工粘贴的凭据常常只有一把 accessToken，这时 `refreshToken` / `expiresAt`
/// 之类的空值**不能**写进已有记录（等于把上次登录拿到的续期串洗掉）。
fn merge_credential(expect: &Value, renewed: &Credential) -> Value {
    let mut fields: Vec<(&str, Value)> = Vec::new();
    for (key, value) in renewed.patch_fields() {
        if value.is_null() || matches!(&value, Value::String(text) if text.is_empty()) {
            continue;
        }
        fields.push((key, value));
    }
    // tokenTail 是展示字段，网关那份也单独写它
    fields.push((
        "tokenTail",
        Value::String(crate::store::token_tail(&renewed.access_token)),
    ));
    crate::store::with_fields(expect, &fields)
}

/// 账号级出口代理（解析失败按 400 报，不静默直连）。
pub(crate) fn account_proxy(record: &Value) -> Result<Option<ResolvedProxy>, GatewayError> {
    match crate::proxy_util::resolve_account_proxy(record.get("proxy")) {
        Some(crate::proxy_util::ProxyResolution::Resolved(proxy)) => Ok(Some(proxy)),
        Some(crate::proxy_util::ProxyResolution::Failed(reason)) => {
            Err(GatewayError::with_status(400, reason))
        }
        None => Ok(None),
    }
}

/// 读账号记录（点名 vs 队首：点名的取不到就报错，不回落到队首 ——
/// 那会把「点名的账号坏了」变成「静默用了别人的额度」）。
///
/// `pub(crate)` 给 `usage.rs` / `checkin.rs` 复用：余额与签到必须读**同一条**
/// 账号记录，两处各写一遍取记录的规定，迟早会出现一边按 id、一边按队首。
pub(crate) async fn read_record(ctx: &PluginCtx, account_id: &str) -> Result<Value, GatewayError> {
    let record = crate::store::record(ctx, PROVIDER_ID, account_id).await?;
    if record.is_null() {
        if account_id.trim().is_empty() {
            return Err(GatewayError::with_status(
                401,
                "没有可用的 Trae 账号，请先在「账号」页添加",
            ));
        }
        return Err(GatewayError::with_status(401, "找不到该 Trae 账号"));
    }
    Ok(record)
}
