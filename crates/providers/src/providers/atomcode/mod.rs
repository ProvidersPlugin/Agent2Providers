//! AtomCode（AtomGit）插件实现：账号管理（**轮询式网页登录**）加推理转发。
//!
//! ── 上游长什么样 ────────────────────────────────────────────
//! 三条平面（细节见各子模块的模块头）：
//!
//! ```text
//!   登录代理（OAuth）  {broker}/auth/{login,check,token} + /oauth/refresh
//!   CodingPlan REST    {codingplan}/coding-plan/{status,models,claim,usage}-v2
//!   聊天网关           {gateway}/chat/completions             OpenAI Chat Completions
//! ```
//!
//! 本家**无状态**（一次 HTTP 请求 = 一次回答，上游就是 OpenAI 协议），
//! `stateful` 恒 false，转发链只需 `build_chat`。
//!
//! ── 四处与别家不同的取舍（都写在各自模块头，这里给索引）─────────
//!   1. **有续期，而且会轮换 refresh token**（`refresh = true`）——
//!      续期必须单飞、必须写回新的一对，见 `oauth.rs` 与模块尾的
//!      `refresh_and_save`；
//!   2. **登录没有本地回调**：授权在代理域名上终结，靠轮询判完成 ——
//!      与 ZCode / OfficeAce 同形，见 `oauth.rs`；
//!   3. **额度是「每窗口调用次数」**，不是积分也不是 token —— 见 `usage.rs`；
//!   4. **网关会把失败包装成成功**：`HTTP 200` + 正文 `参数错误`
//!      （或一页上游 HTML 报错），见 `errors.rs`。
//!
//! ── 登录形态：`poll_interval_ms > 0`（轮询型）────────────────────
//! 本家的授权回调由代理自己在 `acs.atomgit.com/callback` 终结，**永远不回到**
//! 本机，因此 `login_start` 返回的 `poll_interval_ms` **必须大于 0**。
//! 网关据此起一个后台任务按这个间隔问 `login_poll`（见
//! `api::session::start_plugin_web_login` 的分流）—— 报成 0 的症状是
//! 「授权成功、界面一直挂着等一个永远不到的跳转」，且没有任何报错。
//!
//! ── 从网关搬来时改了什么（逐条对照，便于 parity 核对）────────────
//! ```text
//!   不变：三处 body 改写（model 映射 / stream_options / 其余逐字）、
//!         四件功能性请求头、判定顺序即契约的错误分类、档位推断与级联领取、
//!         续期的「非空才覆盖」合并规则、额度窗口归一
//!   改：  ① 凭证从 `store.atomcode_account_record()` 改成 `store::record()`
//!         ② 目录缓存不落盘（网关自己缓存插件返回的清单）
//!         ③ 登录从「网关起任务轮询」改成「插件报 poll_interval_ms +
//!            网关按它调 login_poll」
//!         ④ 轮询拿到令牌后返回 `NormalizedAccount`（落库由网关做）
//! ```
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 本 crate 编译期禁止 unwrap/expect/panic（见 `main.rs` 的 deny）。

pub mod chat;
pub mod checkin;
pub mod credentials;
pub mod endpoints;
pub mod errors;
pub mod models;
pub mod oauth;
pub mod usage;

use std::sync::OnceLock;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ChatPlan, PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::Value;

use crate::errors::GatewayError;
use crate::flight;
use crate::logging;

/// provider id（落盘契约，不能改）。
pub const ID: &str = "atomcode";

/// 排障用的目录条数（`/health` 之类的探针会读）
#[allow(dead_code)]
pub fn model_count() -> usize {
    models::list().len()
}

/// AtomCode 适配器（无状态）。
///
/// 不持有任何数据 —— 凭证在网关的账号库、目录在 `models.rs` 的进程级句柄
/// （可重建的展示数据）。
pub struct AtomcodeProvider;

impl Provider for AtomcodeProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(ID, "AtomCode")
            .with_capabilities(proto::Capabilities {
                chat: true,
                // 一次 HTTP 请求 = 一次回答
                stateful: false,
                // POST {broker}/oauth/refresh（会轮换 refresh token）
                refresh: true,
                // GET {codingplan}/coding-plan/models-v2
                model_refresh: true,
                // GET {codingplan}/coding-plan/status-v2
                usage: true,
                // 签到在这里是「领取免费 CodingPlan」（一次性授权，到期续领）
                checkin: true,
                claims: false,
                // 轮询型网页登录（回调在代理域名终结）
                web_login: true,
                // 上游是 OpenAI Chat，响应的 model 就是请求的那一个
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                refresh_uses_account: true,
                // 本家是否认「默认模型」概念（协议新增位，见 `Capabilities`）
                supports_default_model: false,
                // 本家有没有环境变量 / 默认登录态旁路（协议新增位）
                env_credentials: false,
            })
            .with_login(vec![proto::LoginMethod::Browser {
                button: "浏览器登录".into(),
                hint: Some(
                    "在打开的 AtomGit 授权页完成登录；完成后回到这里等待几秒即可".into(),
                ),
            }])
            // 本家**没有内置兜底清单**（名单里写死的模型点一下可能是「参数错误」，
            // 见 `models.rs` 的模块头），所以默认清单为空 —— 由远程目录填。
            .with_models(Vec::new())
            // 账号行：`usage` —— 额度是 `status-v2` 的**每窗口调用次数**
            // （免费档实测 200 次 / 5 小时），已接 `query_usage`。
            //   `edition: false` —— 单一入口（没有国际版）。
            //   `identifier: userId` —— AtomGit 用户 id（落盘时写在 `userId`）。
            //   `expiry: expiresAt` —— 令牌**不是 JWT**，过期时刻由
            //     `createdAt + expiresIn` 推出后落盘（见 `credentials` 的模块头）。
            .with_account_features(proto::AccountFeatures {
                usage: true,
                identifier: vec!["uid".to_string(), "userId".to_string()],
                expiry: vec!["expiresAt".to_string()],
                ..Default::default()
            })
        .with_checkin(proto::CheckinInfo {
            description: Some("领取免费 CodingPlan（按需：套餐有效时只回读额度）".to_string()),
            modes: vec![proto::CheckinMode::Checkin],
            ..Default::default()
        })
    }

    fn provider_id(&self) -> String {
        ID.to_string()
    }

    // ── 数据面 ────────────────────────────────────────────────

    /// 构造 `POST {网关}/chat/completions`（OpenAI 协议，body 几乎原样透传）。
    ///
    /// 只改 `model` 与 `stream_options` 两处，理由见 `chat.rs` 的模块头。
    fn build_chat<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ChatRequest,
    ) -> SendFuture<'a, ChatPlan> {
        Box::pin(async move {
            let record = crate::store::record(ctx, ID, &req.account).await?;
            let account = crate::store::pick(&record, &["accessToken", "access_token"]);
            let account_shape = serde_json::json!({
                "auth": { "accessToken": account },
            });
            let headers = crate::headers::Headers::from_map(&req.headers);
            chat::build_chat_request(&account_shape, &req.body, &headers)
                .map_err(GatewayError::into_message)
        })
    }

    /// 上游错误分类（判定顺序本身是契约，见 `errors::classify_gateway_error`）：
    ///   1. `ATOMCODE_SIG_MISSING`（打到了需要签名的主机）→ `Fatal`，**先于** 401/403；
    ///   2. 401 / 403 → `TokenExpired`（文案含 `codingplan` 时是「没领套餐」→ `Fatal`）；
    ///   3. 429 → `QuotaLimited`（网关不给结构化恢复时间）；
    ///   4. 其余 → 共用内容拦截分类。
    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        errors::classify_gateway_error(status, body)
    }

    /// 本家把**配额耗尽**包装成一次正常答复：`200` + 一条 SSE 内容帧、正文是
    /// 「模型「xxx」在当前 CodingPlan 用量窗口内已达上限。…」、`finish_reason`
    /// 不存在，随后 `[DONE]`。2026-10-18 在 dev 上逐字抓过（客户端侧 426 字节）。
    ///
    /// 只写**实测见过的那半句**（稳定部分，模型名在「」里会变，所以从「在当前」
    /// 起截）。命中后编排层按**配额**处置：罚账号（冷却到文案里的窗口重置时刻）
    /// + 换家 —— 与内容策略拒绝的「不罚账号」是两条路。
    fn refusal_markers(&self) -> (Vec<String>, Vec<String>) {
        (
            Vec::new(),
            vec!["在当前 CodingPlan 用量窗口内已达上限".to_string()],
        )
    }

    // ── 凭证 ──────────────────────────────────────────────────

    /// 取可用访问令牌。
    ///
    /// ⚠️ **这里不发网络请求**（本方法是取值口，且会在转发路径上被调）：
    /// 临期只记一行提示，真正的续期由维护链路（`refresh_token`）与 401 后的
    /// 强制刷新负责 —— 与 lobsterai 同一取舍。
    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let record = crate::store::record(ctx, ID, account).await?;
            let credentials = credentials::from_record(Some(&record))?;
            if credentials.expiring() {
                logging::verbose(
                    "[AtomCode]",
                    "账号令牌已过期或临期，将按需续期（续期会轮换 refresh token）",
                );
            }
            Ok(credentials.access_token)
        })
    }

    /// 强制续期（401 之后 / 维护链路）：刷新并**把新的一对写回记录**。
    ///
    /// ── 为什么必须写回，以及为什么必须单飞 ───────────────────────
    /// 续期**轮换** refresh token：代理同时下发新 access 与新 refresh，
    /// 旧 access 立即失效。丢掉新的 refresh 等于让账号退化成必须重新登录；
    /// 两个并发续期互相作废（后写覆盖前者）—— 因此走 `flight::Table` 做单飞，
    /// 键含**两个令牌的指纹**（令牌一变就是新的一轮，与 lobsterai 同一口径）。
    ///
    /// ⚠️ **非空才覆盖**：代理可能省略 `refresh_token` / `expires_in` / `user`
    /// （`oauth.rs` 模块头的一手来源）。整包替换会让一次「看起来成功的」续期
    /// 抹掉 refresh，账号从此只能重登。
    fn refresh_token<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, String> {
        Box::pin(async move {
            let record = crate::store::record(ctx, ID, account).await?;
            let credentials = credentials::from_record(Some(&record))?;
            if credentials.refresh_token.is_empty() {
                return Err(GatewayError::with_status(
                    401,
                    "AtomCode 账号没有 refresh_token，无法续期，请重新登录",
                )
                .into_message());
            }
            let key = format!(
                "{}:{}:{}",
                account,
                flight::fingerprint(&credentials.access_token),
                flight::fingerprint(&credentials.refresh_token),
            );
            match ATOMCODE_FLIGHTS
                .get_or_init(flight::Table::new)
                .join(&key)
            {
                flight::Join::Waiter(waiter) => waiter
                    .wait()
                    .await
                    .map(|fresh| fresh.access_token)
                    .map_err(GatewayError::into_message),
                flight::Join::Leader(leader) => {
                    let result = refresh_and_save(ctx, account, &record, &credentials).await;
                    leader.finish(result.clone());
                    result
                        .map(|fresh| fresh.access_token)
                        .map_err(GatewayError::into_message)
                }
            }
        })
    }

    /// 临期判定：`createdAt + expiresIn`（**令牌不是 JWT，读不到 `exp`**），
    /// 提前 [`endpoints::REFRESH_WINDOW_SECONDS`] 报临期。
    fn token_expiring<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, bool> {
        Box::pin(async move {
            if account.is_empty() {
                return Ok(false);
            }
            let Ok(record) = crate::store::record(ctx, ID, account).await else {
                return Ok(false);
            };
            Ok(credentials::from_record(Some(&record))
                .map(|credentials| !credentials.refresh_token.is_empty() && credentials.expiring())
                .unwrap_or(false))
        })
    }

    // ── 目录 / 余额 / 签到 ─────────────────────────────────────

    fn models<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async { Ok(models::list_report()) })
    }

    /// 刷模型目录：用指定账号（空串 = 库里第一个可用账号）的令牌。
    ///
    /// 「一个账号都没有」不是失败 —— 那是「用户还没添加账号」的正常状态，
    /// 返回 `unchanged()`（脚本 / CI 不该看到一个红色失败，与 lobsterai 同口径）。
    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            let record = match crate::store::record(ctx, ID, account).await {
                Ok(record) => record,
                Err(_) => {
                    if account.is_empty() {
                        logging::verbose("[Models]", "AtomCode 模型目录刷新跳过：尚未添加账号");
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
            Ok(models::refresh(&credentials, force).await)
        })
    }

    /// 查额度：`GET {codingplan}/coding-plan/status-v2` 的窗口归一。
    /// 401 由调用方走统一的「刷新后重试」链路。
    fn quota_sources(&self, report: &Value) -> Vec<Value> {
        crate::quota::sources(&self.meta().id, report)
    }

    fn usage<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, Value> {
        Box::pin(async move {
            usage::query_usage(ctx, account)
                .await
                .map_err(GatewayError::into_message)
        })
    }

    /// 「签到」= **领取免费 CodingPlan**（按需领取：先探测，无效才级联）。
    ///
    /// 三种结局对应三种 outcome：
    ///   - 已经有效 → `Done`（今天不需要做事，不谎报成功）；
    ///   - 真领到 → `Claimed`；
    ///   - 三档都被拒 → `Inactive`（常见成因是不在活动期 / 名额已满）。
    fn checkin<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(async move {
            let claim = checkin::claim_plan_if_needed(ctx, account)
                .await
                .map_err(GatewayError::into_control_message)?;
            Ok(checkin_report_of(&claim))
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    /// 添加账号：手填粘贴令牌包。
    ///
    /// 本家**没有**文件导入形态（官方 `auth.toml` 是二进制之外的纯文本，但用户
    /// 手里通常是登录输出的一段 JSON）；`Desktop` 也不适用（AtomCode 没有
    /// 本机登录态文件可读）。两种情况都要给出可读的回绝理由，而不是静默失败。
    fn normalize_account<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move { normalize(input).map_err(GatewayError::into_message) })
    }

    fn account_view(&self, secret: &Value) -> Result<Value, String> {
        Ok(account_view(secret))
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(identity_of(secret))
    }

    // ── 登录（轮询型）──────────────────────────────────────────

    /// 发起登录：先向代理要 `login_url` + `state`。
    ///
    /// ── 为什么 `poll_interval_ms` 必须大于 0 ──────────────────
    /// 授权回调在 `acs.atomgit.com/callback` 终结，**永远不回到本机**。报成 0
    /// 会让网关去等一个不存在的回调（症状见模块头）。
    ///
    /// 间隔取 3 秒：代理那边是浏览器里的一步人工操作，3 秒足够灵敏又不会打得太密
    /// （与内置版 `core::login::atomcode` 的 `POLL_INTERVAL` 同值）。
    fn login_start<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        method: &'a str,
        _payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(async move {
            if method != "browser" && !method.is_empty() {
                return Err(format!("AtomCode 不支持这种登录方式：{method}"));
            }
            let login = oauth::BrokerLogin::start(300_000).await?;
            Ok(proto::LoginStart {
                // `flow` 必须能被 `login_poll` 认出来 —— 用代理给的 state
                flow: login.state().to_string(),
                url: login.login_url().to_string(),
                instructions:
                    "在打开的页面里完成 AtomGit 授权；完成后回到这里，网关会自动取回凭证"
                        .to_string(),
                poll_interval_ms: 3_000,
                external_browser: false,
                allowed_hosts: Vec::new(),
            })
        })
    }

    /// 轮询一次「用户授权完了没」，完成时把令牌包换成 `NormalizedAccount`。
    ///
    /// ── 轮询状态怎么保存 ──────────────────────────────────────
    /// 不需要保存：代理那边以 `state` 为索引记着这一轮，插件只要拿 `flow`
    /// （就是 state）去问就行。所以这里每次都现造一个 `BrokerLogin` 壳
    /// —— 它只是「用 state 问代理」的一个便捷封装。
    ///
    /// ── 网络类失败当 pending ──────────────────────────────────
    /// `BrokerLogin::check` 已经把 5xx / 非 2xx / 畸形体折成 `Ok(false)`：
    /// 那是「这次没问成」而不是「问成了但被拒」，把它当致命错误会让一次抖动
    /// 打断用户正在进行的授权。
    fn login_poll<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async move {
            let login = oauth::BrokerLogin::resume(flow.to_string());
            match login.check().await {
                Ok(false) => Ok(proto::LoginPoll::Pending { hint: None }),
                Ok(true) => match login.fetch_token().await {
                    Ok(bundle) => Ok(proto::LoginPoll::Done {
                        account: account_of(bundle),
                    }),
                    Err(error) => Ok(proto::LoginPoll::Failed {
                        message: error.message,
                    }),
                },
                Err(error) => Ok(proto::LoginPoll::Failed {
                    message: error.message,
                }),
            }
        })
    }
}

/// 续期单飞表（进程级；键含两个令牌的指纹，令牌一变就是新的一轮）
static ATOMCODE_FLIGHTS: OnceLock<flight::Table<credentials::AtomcodeCredentials>> =
    OnceLock::new();

/// 真的去打一次续期，并把**轮换后的令牌对**写回账号记录。
///
/// 抽成自由函数是为了让单飞分支的「Leader 职责」只出现在一处：
/// 发请求 → 合并（非空才覆盖）→ 落盘 → 返回新凭证。
async fn refresh_and_save(
    ctx: &PluginCtx,
    account: &str,
    record: &Value,
    credentials: &credentials::AtomcodeCredentials,
) -> Result<credentials::AtomcodeCredentials, GatewayError> {
    let bundle = oauth::refresh(&credentials.refresh_token).await?;
    // 轮换语义：非空才覆盖（代理可能省略 refresh_token / expires_in / user
    // —— 见 oauth.rs 的模块头，整包替换会让账号失去续期能力）
    let previous = credentials::TokenBundle {
        access_token: credentials.access_token.clone(),
        refresh_token: credentials.refresh_token.clone(),
        ..Default::default()
    };
    let merged = bundle.merge_into(&previous);
    // 落盘失败**不**把这次续期当成功：新的 access 已经在手上、但记录里还是旧的，
    // 下次仍会走一次续期（而旧的 refresh 可能已被轮换作废）。如实上报。
    let expires_at = merged.expires_at(crate::http::now_ms());
    let mut next_secret = crate::store::with_fields(
        record,
        &[
            ("accessToken", Value::String(merged.access_token.clone())),
            (
                "refreshToken",
                Value::String(merged.refresh_token.clone()),
            ),
        ],
    );
    if expires_at > 0 {
        next_secret =
            crate::store::with_fields(&next_secret, &[("expiresAt", Value::from(expires_at))]);
    }
    // 比较再写：`expect` 是读到的那份记录（`store::record` 拼出来的形态），
    // 网关比的是它存的 secret，所以两边都要还原成 secret 形态
    let expect = secret_of(record);
    let written = crate::store::save(ctx, account, &expect, &secret_of(&next_secret)).await?;
    if !written {
        // 账号记录已变（另一次刷新先落盘 / 用户重登）：交给调用方重读
        return Err(GatewayError::with_status(
            502,
            "AtomCode 续期成功但写回账号失败（账号记录已变）：请重试",
        ));
    }
    logging::log("[AtomCode]", "令牌已续期并写回账号（refresh token 已轮换）");
    Ok(credentials::AtomcodeCredentials {
        id: credentials.id.clone(),
        access_token: merged.access_token,
        refresh_token: merged.refresh_token,
        expires_at,
        user_id: merged.user_id,
        name: merged.name,
    })
}

/// 记录形态 → secret 形态（去掉 `store::record` 补上的三个键）。
///
/// 与 lobsterai 的同名函数同一理由：写回时的 `expect` / `next` 必须是网关存的
/// secret 本体，多出来的键会让「比较再写」永远不等。
fn secret_of(record: &Value) -> Value {
    // 去掉 `store::record_of` 补上的三个键（`id` / `provider` / `name`）——
    // 少去掉一个，"比较再写"就会永远不等（网关存的 secret 里没有它们）。
    crate::store::secret_view(record)
}

/// 令牌包 → 可落库的账号。
///
/// ── identity 为什么取 `user_id` ─────────────────────────────
/// AtomGit 的用户 id 是稳定不可变的（登录名可能改）。同一账号重复登录会被
/// 识别成更新而不是新增 —— 与内置版 `add_atomcode_account` 的去重口径一致。
fn account_of(bundle: credentials::TokenBundle) -> proto::NormalizedAccount {
    let name = credentials::display_name(&bundle);
    let created_at = crate::http::now_ms() / 1000;
    let mut secret = serde_json::json!({
        "accessToken": bundle.access_token,
        "createdAt": created_at,
        "expiresIn": bundle.expires_in,
    });
    if !bundle.refresh_token.trim().is_empty() {
        secret["refreshToken"] = Value::String(bundle.refresh_token.clone());
    }
    if !bundle.user_id.trim().is_empty() {
        secret["userId"] = Value::String(bundle.user_id.clone());
    }
    if !bundle.username.trim().is_empty() {
        secret["username"] = Value::String(bundle.username.clone());
    }
    if !bundle.name.trim().is_empty() {
        secret["nickname"] = Value::String(bundle.name.clone());
    }
    if !bundle.email.trim().is_empty() {
        secret["email"] = Value::String(bundle.email.clone());
    }
    if !bundle.avatar_url.trim().is_empty() {
        secret["avatarUrl"] = Value::String(bundle.avatar_url.clone());
    }
    secret["name"] = Value::String(name.clone());
    proto::NormalizedAccount {
        public: account_view(&secret),
        secret,
        identity: identity_of(&serde_json::json!({ "userId": bundle.user_id })),
        desktop: false,
    }
}

/// 账号的稳定身份：`user-<userId>`（没有 userId 时退回用户名，都没有给空串
/// 让网关按凭证去重）。
fn identity_of(secret: &Value) -> String {
    let user_id = crate::store::pick(secret, &["userId", "user_id", "uid"]);
    if !user_id.is_empty() {
        return format!("user-{user_id}");
    }
    let username = crate::store::pick(secret, &["username", "userName"]);
    if !username.is_empty() {
        return format!("user-{username}");
    }
    String::new()
}

/// 账号公开视图（界面账号页读的那几个键）。
fn account_view(secret: &Value) -> Value {
    let access_token = crate::store::pick(secret, &["accessToken", "access_token", "token"]);
    let user_id = crate::store::pick(secret, &["userId", "user_id", "uid"]);
    let name = {
        let explicit = crate::store::pick(secret, &["name"]);
        if !explicit.is_empty() {
            explicit
        } else {
            let username = crate::store::pick(secret, &["username", "userName"]);
            if !username.is_empty() {
                username
            } else if !user_id.is_empty() {
                format!("AtomGit 账号 {user_id}")
            } else {
                "AtomCode 账号".to_string()
            }
        }
    };
    let mut view = serde_json::json!({
        "name": name,
        "uid": user_id,
        "tokenTail": crate::store::token_tail(&access_token),
        "hasSession": true,
        "canRefresh": !crate::store::pick(secret, &["refreshToken", "refresh_token"]).is_empty(),
    });
    let expires_at = credentials::from_record(Some(secret))
        .map(|credentials| credentials.expires_at)
        .unwrap_or(0);
    if expires_at > 0 {
        if let Some(object) = view.as_object_mut() {
            object.insert("expiresAt".to_string(), Value::from(expires_at));
        }
    }
    if let Some(object) = view.as_object_mut() {
        let email = crate::store::pick(secret, &["email"]);
        if !email.is_empty() {
            object.insert("email".to_string(), Value::String(email));
        }
        let avatar = crate::store::pick(secret, &["avatarUrl", "avatar_url"]);
        if !avatar.is_empty() {
            object.insert("avatar".to_string(), Value::String(avatar));
        }
    }
    view
}

/// 签到结果 → 协议化的 `CheckinReport`。
///
/// `checkin.rs` 给的是内置版的 `{success, msg, needsAction, ...}` 形态：
///   - `success: true` → 真领到 → `Claimed`；
///   - `needsAction: false` → 套餐有效 / 已持有 → `Done`（今天不需要做事）；
///   - 其余 → `Inactive`（三档都被拒，常见成因是不在活动期）。
fn checkin_report_of(claim: &Value) -> proto::CheckinReport {
    let success = claim
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let needs_action = claim
        .get("needsAction")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let message = claim
        .get("msg")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let credit = claim
        .get("tier")
        .and_then(Value::as_str)
        .map(|tier| format!("{tier} 档免费套餐"));
    let outcome = if success {
        proto::CheckinOutcome::Claimed
    } else if !needs_action {
        proto::CheckinOutcome::Done
    } else {
        proto::CheckinOutcome::Inactive
    };
    proto::CheckinReport {
        outcome,
        credit,
        streak: None,
        message,
    }
}

// ─── 账号规范化 ────────────────────────────────────────────────

/// 把粘贴的令牌包变成一条可落库的账号。
///
/// ── 为什么文件 / 桌面端两种来源要回绝而不是硬凑 ────────────────
/// AtomCode 的凭证是代理下发的一对令牌，既没有「客户端导出的凭证文件」这个
/// 约定（官方 `auth.toml` 是它自己 CLI 的落盘形态，字段与我们的记录不同），
/// 也没有可实时读取的本机登录态。硬凑出来的实现只会在用户点下去之后失败，
/// 现在就说清「请用浏览器登录」反倒省事。
fn normalize(input: proto::NormalizeInput) -> Result<proto::NormalizedAccount, GatewayError> {
    match input.source {
        proto::NormalizeSource::Manual { payload } => from_payload(&payload),
        proto::NormalizeSource::FileText { content, .. } => {
            let parsed: Value = serde_json::from_str(&content).map_err(|_| {
                GatewayError::with_status(400, "凭证文件不是合法 JSON（应粘贴登录输出）")
            })?;
            from_payload(&parsed)
        }
        proto::NormalizeSource::FileBase64 { .. } => Err(GatewayError::with_status(
            400,
            "AtomCode 不支持二进制凭证文件，请改用浏览器登录",
        )),
        proto::NormalizeSource::Desktop => Err(GatewayError::with_status(
            400,
            "AtomCode 没有可导入的本机登录态，请改用浏览器登录",
        )),
    }
}

/// 从一份 JSON 载荷里解析账号（登录输出 / 手填粘贴共用）。
fn from_payload(payload: &Value) -> Result<proto::NormalizedAccount, GatewayError> {
    // 直接粘一串令牌：只可能是 access_token（没有 refresh 的话能转发但不能续期）
    let raw = payload.as_str().map(str::trim).unwrap_or("");
    if !raw.is_empty() {
        let secret = serde_json::json!({
            "accessToken": raw,
            "name": "AtomCode 账号",
        });
        return Ok(proto::NormalizedAccount {
            public: account_view(&secret),
            identity: identity_of(&secret),
            secret,
            desktop: false,
        });
    }
    let access_token = crate::store::pick(payload, &["accessToken", "access_token", "token"]);
    if access_token.is_empty() {
        return Err(GatewayError::with_status(
            400,
            "缺少 accessToken（请粘贴浏览器登录后的登录输出）",
        ));
    }
    let mut secret = payload.clone();
    if let Some(object) = secret.as_object_mut() {
        object.insert(
            "accessToken".to_string(),
            Value::String(access_token.clone()),
        );
        // token_type 恒 Bearer，不落（没有消费方）
        object.remove("token_type");
        object.remove("tokenType");
    }
    let name = {
        let explicit = input_name(payload);
        if explicit.is_empty() {
            let username = crate::store::pick(payload, &["username", "userName"]);
            if username.is_empty() {
                "AtomCode 账号".to_string()
            } else {
                username
            }
        } else {
            explicit
        }
    };
    secret["name"] = Value::String(name);
    Ok(proto::NormalizedAccount {
        public: account_view(&secret),
        identity: identity_of(&secret),
        secret,
        desktop: false,
    })
}

/// 载荷里自带的备注名（`name`）—— 只有非空且不是被我们写进去的默认值时才用。
fn input_name(payload: &Value) -> String {
    crate::store::pick(payload, &["name"])
}
