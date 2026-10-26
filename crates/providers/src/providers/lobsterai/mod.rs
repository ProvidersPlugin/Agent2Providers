//! LobsterAI（网易有道「有道龙虾」）插件实现：账号管理（浏览器登录）+ **无状态**
//! 推理转发 + 目录 / 余额 / 每日签到。
//!
//! ── 上游长什么样（细节见各子模块的模块头）──────────────────────
//! ```text
//!   登录门户   https://lobsterai.youdao.com        浏览器授权页（手机号 / 微信）
//!   API        https://lobsterai-server.youdao.com  换码 / 续期 / 目录 / 对话 / 积分 / 活动
//! ```
//! 对话端点是 **OpenAI 兼容的原生协议**且**只收 SSE**（`stream:false` 回
//! HTTP 500，`endpoints.rs` 的模块头有实测）：本网关的转发入口恒以 `stream:true`
//! 请求上游，所以这条要求天然满足。本家**无状态**（一次 HTTP 请求 = 一次回答），
//! `stateful` 恒 false，转发链只需 `build_chat`。
//!
//! ── 从网关搬来时改了什么（逐条对照，便于 parity 核对）────────────
//! ```text
//!   不变：四处 body 改写（developer→system / tool_choice / prompt_cache_key /
//!         模型名解析）、五条功能性头（含两个 X-LobsterAI-Client-*）、
//!         判定顺序即契约的错误分类、19 条静态兜底目录、客户端版本解析与
//!         12 小时缓存、三步签到协议与两道预检、积分读数的归一形状
//!   改：  ① 凭证从 `store.lobsterai_account_record()` 改成 `store::record()`
//!            —— 插件架构下账号库在网关侧，插件只能走回调；
//!         ② 目录缓存**不落盘**（网关自己会缓存插件返回的清单，见 `models.rs`）；
//!         ③ loopback 端口从 `HostEnv.loopback_port` 取（网关握手时给）；
//!         ④ 登录的「落账号」从 `add_lobsterai_account` 改成返回
//!            `NormalizedAccount` —— 账号库的写入由网关做（`login.callback` 的
//!            应答由 `RpcAdapter::save_login_account` 落地）；
//!         ⑤ 签到返回协议化的 `CheckinReport`（网关再翻成 `{success, msg}`）。
//! ```
//!
//! ── 五处与别家不同的取舍（都写在各自模块头，这里给索引）─────────
//!   1. **登录是「回调带回一次性 code」那一套**（`web_login = true`）：
//!      授权在门户完成、302 回本机 loopback，插件用 code 换令牌（`oauth.rs`）。
//!   2. **续期要一整组身份载荷件**（`uuid` + 两个 keyfrom），且
//!      `latestKeyfrom` **永不更新** —— 只存两个 token 的账号续不了期
//!      （`credentials.rs` 的模块头有这条约束的一手来源）。
//!   3. **额度是积分**，读数在 `/api/user/profile-summary`（**不是**
//!      `/api/user/quota`，那个只报 300 免费额度）—— 见 `usage.rs`。
//!   4. **签到是三步协议且幂等靠客户端**（活动位 → 上下文两道预检 →
//!      check_in + uuid4 幂等键）—— 见 `checkin.rs`。
//!   5. **错误分类里「余额耗尽」是最主要的失败模式**，而且它可能以
//!      `HTTP 200` + 流内错误帧下发 —— 见 `errors.rs`（含一处**已知缺口**）。
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
pub mod version;

use std::sync::OnceLock;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::{Map, Value};

use crate::errors::GatewayError;
use crate::flight;
use crate::logging;

/// provider id（落盘契约，不能改）。
pub const ID: &str = "lobsterai";

/// LobsterAI 适配器（无状态）。
pub struct LobsterAiProvider;

impl Provider for LobsterAiProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(ID, "LobsterAI")
            .with_capabilities(proto::Capabilities {
                chat: true,
                // 一次 HTTP 请求 = 一次回答
                stateful: false,
                // 有 /api/auth/refresh 续期接口
                refresh: true,
                // 有 /api/models/available 目录接口
                model_refresh: true,
                // 有 /api/user/profile-summary 积分读数
                usage: true,
                checkin: true,
                claims: false,
                // 授权页 → 本机 loopback → 插件换码
                web_login: true,
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
                hint: Some("打开 LobsterAI 授权页，登录后自动回填账号".into()),
            }])
            .with_models(models::raw_list())
            // 账号行：`usage` —— 余额是**积分**（`GET /api/user/profile-summary`）。
            //   `credit_detail` —— 同一份 profile-summary 还带**逐份积分包**
            //     （面额 / 已用 / 剩余 / 到期），正是 `credits[]` 那行的形状；
            //     与 ZCode 的纯比例窗口不同，这里每份都有绝对值。⚠️ 这一位是
            //     **承诺**：`query_usage` 不出 `credits` 时入口不挂，所以现在
            //     开它是安全的。
            //   `edition: false` —— 单一站点（`lobsterai.youdao.com`）。
            //   `identifier: userId` —— 身份键取 `userId`（落账号时的口径是
            //     `userId → uid → 令牌尾段`，账号 id 也由它拼成）。
            //   `expiry: expiresAt` —— 落盘写的是毫秒（显式 > createdAt +
            //     expiresIn > JWT 的 `exp`），与 atomcode 同一条教训。
            .with_account_features(proto::AccountFeatures {
                usage: true,
                credit_detail: true,
                identifier: vec!["userId".to_string(), "uid".to_string()],
                expiry: vec!["expiresAt".to_string()],
                ..Default::default()
            })
        .with_checkin(proto::CheckinInfo {
            description: Some("每日活动签到（+100 积分，已领则中性提示）".to_string()),
            modes: vec![proto::CheckinMode::Checkin],
            ..Default::default()
        })
    }

    fn provider_id(&self) -> String {
        ID.to_string()
    }

    // ── 数据面 ────────────────────────────────────────────────

    /// 构造 `POST {api}/api/proxy/v1/chat/completions`（OpenAI 协议，body 只改
    /// 四处，见 `chat.rs` 的模块头）。
    fn build_chat<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ChatRequest,
    ) -> SendFuture<'a, agent2api_provider_sdk::ChatPlan> {
        Box::pin(async move {
            let record = crate::store::record(ctx, ID, &req.account).await?;
            let account = account_shape(&record);
            let headers = crate::headers::Headers::from_map(&req.headers);
            chat::build_chat_request(&account, &req.body, &headers)
                .map_err(GatewayError::into_message)
        })
    }

    /// 上游错误分类（判定顺序本身是契约，见 `errors::classify_gateway_error`）：
    ///   1. 402 / 正文含余额耗尽关键词 → `QuotaLimited`（换号 + 冷却）；
    ///   2. 40100 / 40101 → `TokenExpired`（只能重新登录）；
    ///   3. 429 → `QuotaLimited`；404 / 5xx → `Fatal`；
    ///   4. 其余 4xx → 共用内容拦截分类。
    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        errors::classify_gateway_error(status, body)
    }

    // ── 凭证 ──────────────────────────────────────────────────

    /// 取可用访问令牌。
    ///
    /// ⚠️ **这里不发网络请求**（本方法是取值口，且会在转发路径上被调）：
    /// 临期只记一行提示，真正的续期由维护链路（`refresh` +
    /// `refresh_token`）与 401 后的强制刷新负责 —— 与 `atomcode/mod.rs`
    /// 同一取舍。
    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let record = crate::store::record(ctx, ID, account).await?;
            let credentials = credentials::from_record(Some(&record))?;
            if credentials.expiring() {
                logging::verbose(
                    "[LobsterAI]",
                    "账号令牌已过期或临期，将按需续期（续期会轮换 refresh token）",
                );
            }
            Ok(credentials.access_token)
        })
    }

    /// 强制续期（401 之后 / 维护链路）：刷新并**把新令牌写回记录**。
    ///
    /// ── 为什么必须写回，以及为什么必须单飞 ───────────────────────
    /// 续期**会轮换** refresh token：上游同时下发新 access 与新 refresh，旧
    /// access 立即失效。丢掉新的 refresh 等于让账号退化成必须重新登录；两个并发
    /// 续期互相作废（后写覆盖前者）—— 因此走 `flight::Table` 做单飞，
    /// 键含**两个令牌的指纹**（令牌一变就是新的一轮，与 Cline 同一口径）。
    ///
    /// ⚠️ **`latestKeyfrom` 绝不覆盖**：续期只回写 access / refresh / expiresAt，
    /// 载荷件三件套保持登录时那一份（`credentials.rs` 的模块头）。
    fn refresh_token<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, String> {
        Box::pin(async move {
            let record = crate::store::record(ctx, ID, account).await?;
            let credentials = credentials::from_record(Some(&record))?;
            if credentials.refresh_token.is_empty() {
                return Err(
                    GatewayError::with_status(
                        401,
                        "LobsterAI 账号没有 refresh_token，无法续期，请重新登录",
                    )
                    .into_message(),
                );
            }
            let key = format!(
                "{}:{}:{}",
                account,
                flight::fingerprint(&credentials.access_token),
                flight::fingerprint(&credentials.refresh_token),
            );
            match LOBSTERAI_FLIGHTS
                .get_or_init(flight::Table::new)
                .join(&key)
            {
                flight::Join::Waiter(waiter) => {
                    waiter.wait().await.map(|fresh| fresh.access_token).map_err(GatewayError::into_message)
                }
                flight::Join::Leader(leader) => {
                    let result = refresh_and_save(ctx, account, &record, &credentials).await;
                    leader.finish(result.clone());
                    result.map(|fresh| fresh.access_token).map_err(GatewayError::into_message)
                }
            }
        })
    }

    /// 临期判定：`expiresAt`（落盘时算好）或 JWT 的 `exp`，到期前 1 小时算临期
    /// （见 `credentials::LobsterCredentials::expiring`）。
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
        Box::pin(async { Ok(models::list()) })
    }

    /// 刷模型目录：用指定账号（空串 = 库里第一条启用的账号）的令牌。
    ///
    /// 「一个账号都没有」不是失败 —— 那是「用户还没添加账号」的正常状态，
    /// 返回 `unchanged()`（脚本 / CI 不该看到一个红色失败，与 AtomCode / Loomy
    /// 同口径）。
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
                        logging::verbose("[Models]", "LobsterAI 模型目录刷新跳过：尚未添加账号");
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

    fn quota_sources(&self, report: &Value) -> Vec<Value> {
        crate::quota::sources(&self.meta().id, report)
    }

    fn usage<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move { usage::query_usage(ctx, account).await.map_err(GatewayError::into_message) })
    }

    /// 每日签到（三步协议）。
    ///
    /// 协议化的结果见 `CheckinOutcome` 的六档：本家只会给出
    /// `Claimed`（真签上）/ `Done`（今天已签）/ `Inactive`（活动不可签）/
    /// `Failed`（信封层失败）。
    fn checkin<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(async move {
            let outcome = checkin::claim_daily_checkin(ctx, account)
                .await
                .map_err(GatewayError::into_control_message)?;
            Ok(checkin_report_of(&outcome))
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    /// 手动添加：**如实拒绝**（与内置 `api::accounts` 的 LobsterAI 分支同口径）。
    ///
    /// ── 为什么不能「尽力而为」──────────────────────────────────
    /// 本家的续期要一整组身份载荷件（`uuid` + 两个 keyfrom），它们只在换码
    /// 那一刻由上游下发 —— 用户从别处粘不出这三样（`credentials.rs` 的模块头
    /// 是一手来源）。没有它们账号只能用到 access 过期，之后必须重新登录。
    /// 与其落一条看着正常、到期就废的记录，不如现在就说清只有网页登录一条路。
    fn normalize_account<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async {
            Err(
                "LobsterAI 只能通过「网页登录」添加：它的续期需要换码时下发的 \
                 uuid 与 keyfrom，粘贴给不出这三项"
                    .to_string(),
            )
        })
    }

    /// 账号视图：把网关记录形态拆成「公开视图 + 身份」。
    fn account_view(&self, secret: &Value) -> Result<Value, String> {
        Ok(credentials::account_view(secret))
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(credentials::identity_of(secret))
    }

    // ── 登录 ──────────────────────────────────────────────────

    /// 生成授权地址（回调型：`poll_interval_ms = 0`）。
    ///
    /// 端口来自 `HostEnv.loopback_port`（网关握手时给）。端口未知时返回
    /// 「拿不到回调地址」的错误 —— 上层文案会把原因说清，比拼一个必然 404
    /// 的地址好。
    fn login_start<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        method: &'a str,
        _payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(async move {
            if method != "browser" && !method.is_empty() {
                return Err(format!("LobsterAI 不支持这种登录方式：{method}"));
            }
            let Some(return_url) = oauth::redirect_uri(ctx) else {
                return Err("网关尚未报告监听端口，无法生成 LobsterAI 授权地址".to_string());
            };
            let (auth_url, state) = oauth::begin_login(&return_url);
            Ok(proto::LoginStart {
                flow: state,
                url: auth_url,
                instructions: "在打开的页面里完成登录，成功后会自动回到应用".to_string(),
                poll_interval_ms: 0,
                external_browser: false,
                allowed_hosts: vec![
                    "lobsterai.youdao.com".to_string(),
                    "126.com".to_string(),
                    "163.com".to_string(),
                    "weixin.qq.com".to_string(),
                    "open.weixin.qq.com".to_string(),
                ],
            })
        })
    }

    /// 用回调里的一次性 `code` 换令牌。
    ///
    /// `flow` 就是 `login.start` 返回的 state：先取走那一轮（取走即失效），
    /// 再换码。`uuid` 与两个 keyfrom 由**插件生成**（参考实现同款）：`uuid`
    /// 是这次登录的安装标识、`firstKeyfrom` 是登录时刻（毫秒）、
    /// `latestKeyfrom` 取当前时刻 —— 三者随换码下发、续期时要原样回传。
    fn login_callback<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        callback: proto::LoginCallback,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async move {
            // `code` 与 `url` 都可能带着完整回调串（网关的两条入口给的不同）
            let raw = if callback.url.trim().is_empty() {
                callback.code.clone()
            } else {
                callback.url.clone()
            };
            let code = callback_code(&raw, &callback.code);
            if code.trim().is_empty() {
                return Ok(proto::LoginPoll::Failed {
                    message: "回调里没有授权码，请重新发起登录".to_string(),
                });
            }
            if oauth::take_pending(&callback.flow).is_none() {
                return Ok(proto::LoginPoll::Failed {
                    message: "这次登录已取消或已过期，请重新发起".to_string(),
                });
            }
            let client_version = chat::prime_client_version().await;
            let keyfrom = crate::http::now_ms().max(0).to_string();
            let uuid = crate::ids::uuid_v4();
            let bundle =
                match oauth::exchange(&code, &uuid, &keyfrom, &keyfrom, &client_version).await {
                    Ok(bundle) => bundle,
                    Err(error) => {
                        return Ok(proto::LoginPoll::Failed { message: error.message });
                    }
                };

            // 落账号的字段用本仓驼峰名（与内置版记录的形态一致，便于人工核对）
            let mut secret = Map::new();
            secret.insert(
                "accessToken".to_string(),
                Value::String(bundle.access_token.clone()),
            );
            if !bundle.refresh_token.trim().is_empty() {
                secret.insert(
                    "refreshToken".to_string(),
                    Value::String(bundle.refresh_token.clone()),
                );
            }
            secret.insert("uuid".to_string(), Value::String(uuid));
            secret.insert("firstKeyfrom".to_string(), Value::String(keyfrom.clone()));
            secret.insert("latestKeyfrom".to_string(), Value::String(keyfrom));
            if !bundle.uid.trim().is_empty() {
                secret.insert("uid".to_string(), Value::String(bundle.uid.clone()));
            }
            if !bundle.user_id.trim().is_empty() {
                secret.insert("userId".to_string(), Value::String(bundle.user_id.clone()));
            }
            if !bundle.nickname.trim().is_empty() {
                secret.insert("name".to_string(), Value::String(bundle.nickname.clone()));
            }
            if bundle.expires_in > 0 {
                secret.insert("expiresIn".to_string(), Value::from(bundle.expires_in));
            }
            let secret = Value::Object(secret);
            let public = credentials::account_view(&secret);
            let identity = credentials::identity_of(&secret);
            Ok(proto::LoginPoll::Done {
                account: proto::NormalizedAccount {
                    secret,
                    public,
                    identity,
                    desktop: false,
                },
            })
        })
    }
}

/// 续期单飞表（进程级；键含两个令牌的指纹，令牌一变就是新的一轮）
static LOBSTERAI_FLIGHTS: OnceLock<flight::Table<credentials::LobsterCredentials>> =
    OnceLock::new();

/// 真的去打一次续期，并把**轮换后的令牌**写回账号记录。
///
/// ── 为什么只改三个字段 ──────────────────────────────────────
/// 载荷件三件套（uuid / 两个 keyfrom）**绝不能动**：它们是登录那一刻的形态，
/// 续期改写会让下一次续期的请求体与上游期望不符（见 `credentials.rs` 的模块头）。
/// 所以走 `store::with_fields` 而不是整份替换。
async fn refresh_and_save(
    ctx: &PluginCtx,
    account: &str,
    record: &Value,
    credentials: &credentials::LobsterCredentials,
) -> Result<credentials::LobsterCredentials, GatewayError> {
    let client_version = chat::prime_client_version().await;
    let bundle = oauth::refresh(credentials, &client_version).await?;
    // 过期时刻：刚续期成功，以此刻为基准算（`expires_in` 缺失时退回读 JWT 的 exp）
    let expires_at =
        credentials::expires_at_from(crate::http::now_ms(), bundle.expires_in, &bundle.access_token);
    // 轮换语义：refresh token 非空才算轮换（上游可能省略，那表示沿用旧的那一个）
    let rotated = !bundle.refresh_token.trim().is_empty();
    let next = crate::store::with_fields(
        record,
        &[
            ("accessToken", Value::String(bundle.access_token.clone())),
            (
                "refreshToken",
                Value::String(if rotated {
                    bundle.refresh_token.clone()
                } else {
                    credentials.refresh_token.clone()
                }),
            ),
            ("expiresAt", Value::from(expires_at)),
        ],
    );
    // 比较再写：`expect` 是读到的那份记录（`store::record` 拼出来的形态）。
    // 网关的 `plugin_update_secret` 比的是**它存的 secret**，所以这里要把
    // 记录形态还原成 secret 形态（去掉 id / provider / name）。
    let expect = secret_of(record);
    let written = crate::store::save(ctx, account, &expect, &secret_of(&next)).await?;
    if !written {
        return Err(GatewayError::with_status(
            502,
            "LobsterAI 续期成功但写回账号失败（账号记录已变）：请重试",
        ));
    }
    logging::log("[LobsterAI]", "令牌已续期并写回账号（refresh token 已轮换）");
    Ok(credentials::LobsterCredentials {
        id: credentials.id.clone(),
        access_token: bundle.access_token.clone(),
        refresh_token: if rotated {
            bundle.refresh_token
        } else {
            credentials.refresh_token.clone()
        },
        expires_at,
        // 载荷件三件套保持登录时那一份（**不覆盖**）
        uid: credentials.uid.clone(),
        user_id: credentials.user_id.clone(),
        nickname: credentials.nickname.clone(),
        uuid: credentials.uuid.clone(),
        first_keyfrom: credentials.first_keyfrom.clone(),
        latest_keyfrom: credentials.latest_keyfrom.clone(),
    })
}

/// 记录形态 → secret 形态（去掉 `store::record` 补上的三个键）。
///
/// ── 为什么需要这个往返 ──────────────────────────────────────
/// [`crate::store::record`] 把 `secret` 摊平并补了 `id` / `provider` / `name`
/// （让搬来的 `from_record` 逐字可用）。而写回时的 `expect` / `next` 必须是
/// **网关存的 secret 本体** —— 多出来的三个键会让「比较」永远不等。
fn secret_of(record: &Value) -> Value {
    // 去掉 `store::record_of` 补上的三个键（`id` / `provider` / `name`）——
    // 少去掉一个，"比较再写"就会永远不等（网关存的 secret 里没有它们）。
    crate::store::secret_view(record)
}

/// 把 `store::record` 的记录形态还原成 `build_chat_request` 期望的账号形态。
///
/// ── 为什么套一层 `auth` ────────────────────────────────────
/// 搬来的 `chat.rs` 读的是 `account["auth"]["accessToken"]`（那是网关会话的
/// 形态，内置版把记录拼成会话后才交给适配器）。插件拿到的记录是**平铺**的，
/// 所以这里补一层壳 —— 让 `chat.rs` 一行都不用改，parity 核对时逐字可比。
fn account_shape(record: &Value) -> Value {
    let token = crate::store::pick(record, &["accessToken", "access_token"]);
    serde_json::json!({
        "auth": {
            "accessToken": token,
            "tokenType": "Bearer",
        },
    })
}

/// 从回调里取出授权码。
///
/// 两种入口给的东西不同：`login.callback` 的 `code` 已经是纯 code，而
/// `RpcAdapter::exchange_login_code` 会把**整条回调 URL** 放进 `code`
/// （见网关侧那段注释）。两种形态都要认。
fn callback_code(raw: &str, fallback: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return fallback.trim().to_string();
    }
    if !trimmed.contains('=') {
        return trimmed.to_string();
    }
    oauth::parse_callback(trimmed.split_once('?').map(|(_, query)| query).unwrap_or(trimmed))
        .map(|(code, _)| code)
        .unwrap_or_else(|| trimmed.to_string())
}

/// 签到结果 → 协议化的 `CheckinReport`。
///
/// `checkin.rs` 给的是网关内置版的 `{success, msg, alreadyCompleted?}` 形态
/// （那份形状的判据在网关 `billing::checkin::checkin_completed_today` 里）。
/// 过线时翻成六档枚举，网关再翻回它自己的行形状 —— 两端的文案因此保持一致。
fn checkin_report_of(claim: &Value) -> proto::CheckinReport {
    let success = claim.get("success").and_then(Value::as_bool).unwrap_or(false);
    let already = claim
        .get("alreadyCompleted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let message = claim
        .get("msg")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let credit = claim
        .get("credit")
        .and_then(Value::as_f64)
        .filter(|value| *value > 0.0)
        .map(|value| format!("{value} 积分"));
    let outcome = if success {
        proto::CheckinOutcome::Claimed
    } else if already {
        proto::CheckinOutcome::Done
    } else if message.is_some() && !message.as_deref().unwrap_or("").contains("已签到") {
        // 「今天没有可签到的活动」这类中性结果 —— 不是失败，如实报「无活动」
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
