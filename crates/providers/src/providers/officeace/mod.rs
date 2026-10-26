//! OfficeAce（华为云果办 / OfficeClaw）插件实现：**无状态转发 + 两层凭证**。
//!
//! ── 上游长什么样 ────────────────────────────────────────────
//! 上游**本身就是 OpenAI 兼容**：chat 打 `{model_api_url_base}/v2/chat/completions`，
//! 鉴权是一个 Basic 头（`model_app_key`:`model_app_secret`）。桌面端只是把这张表
//! 落在本机 —— 读文件，不是中间人抓包。
//!
//! 凭据**分两层**（别混）：
//! ```text
//!   网关凭据    modelAppKey / modelAppSecret，给 chat 用，**不过期**
//!   控制面凭据  HSTA… 的 AK/SK + securityToken + projectId，**实测 2 小时**，
//!               只用来签控制面（问云端要模型表、领奖励、续期）
//! ```
//! 因此「导入过一次」就有稳定可用的转发账号；控制面过期只影响额度/签到与自动
//! 续期，**不影响转发**。
//!
//! ── 为什么是无状态 ──────────────────────────────────────────
//! 上游模型网关就是 OpenAI chat/completions，一次请求一次回答，没有会话概念。
//! 所以只实现 `build_chat`。
//!
//! ── 三条上游特性（每一条都有实测依据，改之前先读对应模块）────────
//! ```text
//!   1. 上游**恒流式**：请求体强制 stream:true + stream_options.include_usage，
//!      非流式客户端请求由网关聚合（`chat::build_chat_plan`）。
//!   2. `max_tokens` 是「推理 + 正文」的总预算：给小了会 HTTP 200 但正文空串。
//!      这里**追加** 1024（不是取 max），<16 的值原样透传。
//!   3. 体积超限（messages 条数 > 5000）上游给的是 **HTTP 429 + 81113** ——
//!      必须先于限额判定挡掉，否则会白白冷却一个无关的账号（见 `classify`）。
//! ```
//!
//! ── 从网关搬来时改了什么（逐条对照，便于 parity 核对）────────────
//! ```text
//!   不变：请求构造（含推理预算与恒流式）、错误分类码表、models 解析与倍率合并、
//!         签名（V11 SDK-HMAC-SHA256）、订阅 / 奖励 / 新手任务的全部判定
//!   改：  ① 凭证从 `store.officeace_account_record()` 改成 `store::record()`
//!         ② 目录缓存与可用性探测结论不落盘（网关自己缓存；探测重启重跑）
//!         ③ 奖励台账改走 `store.ledger_get/put`
//!         ④ OAuth 自助登录与续期一并搬进来（`oauth`）—— 它是上游协议细节，
//!            留在网关就等于主仓库继续携带「怎么以官方客户端形态换凭据」的知识
//! ```
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 本 crate 编译期禁止 unwrap/expect/panic（见 `main.rs` 的 deny）。

pub mod balance;
pub mod chat;
pub mod checkin;
mod control;
pub mod credentials;
pub mod models;
pub mod oauth;
pub mod onboarding;
pub mod probe;
pub mod signer;
pub mod subscription;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ChatPlan, PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::Value;

use crate::errors::GatewayError;
use crate::headers::Headers;


/// provider id（落盘契约）。
pub const ID: &str = "officeace";

/// OfficeAce 适配器（无状态单例）。
///
/// **不持有任何数据** —— 凭证在账号存储、目录在 `models` 的进程级句柄、
/// 探测结论在 `probe` 的进程级句柄。与网关那份 `OfficeAceAdapter` 逐字同构。
pub struct OfficeAceProvider;

impl Provider for OfficeAceProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(ID, "OfficeAce 果办")
            .with_capabilities(proto::Capabilities {
                chat: true,
                stateful: false,
                // 控制面临时凭据约 2 小时到期，有续期链
                refresh: true,
                // 远程目录（`GET {网关}/v1/models`）
                model_refresh: true,
                // 订阅快照（`GET /v1/subscription`，V11 签名）
                usage: true,
                // `POST /v1/subscription/bonus/claim` 领奖励积分
                checkin: true,
                // 新手任务与签到是**同一个上游动作**（一次 claim 全领）
                claims: true,
                web_login: true,
                // 上游回自己的模型名（不改写也无害，与内置版同一结论）
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                // 没有环境变量旁路
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                refresh_uses_account: true,
                // 本家是否认「默认模型」概念（协议新增位，见 `Capabilities`）
                supports_default_model: false,
                // 本家有没有环境变量 / 默认登录态旁路（协议新增位）
                env_credentials: false,
            })
            .with_login(vec![proto::LoginMethod::Browser {
                button: "网页登录".to_string(),
                hint: Some("完成华为云授权后，网关会自动轮询取回登录凭据".to_string()),
            }])
            .with_models(models::list())
            // 账号行：`usage` —— 额度/积分要**控制面签名**（V11-HMAC-SHA256 打
            // `/v1/subscription`），那条链已实现（`balance` + 适配器
            // `query_usage`），所以余额入口与积分列都开。⚠️ 只导入网关 Basic
            // 那一对、没有控制面凭据的账号查不到余额，后端回 400
            // `usage_not_configured`，界面按「未配置查询」如实显示（不是报错）。
            //   `edition: false` —— 单一入口（自助 OAuth / 粘贴网关 Basic）。
            //   `identifier: id` —— 账号标识渲染的是**账号 id**
            //     （`officeace-<hash>`，由网关基址 + app key 派生），不是任何
            //     凭据本身：面板上显示 `modelAppKey` 会把网关 key 露在界面上。
            //   `expiry: expiresAt` —— 控制面临时凭据约 2 小时到期；只导入网关
            //     Basic 凭据时没有这个字段，界面不渲染有效期（那一半本来也不过期）。
            //   `credit_detail` —— 逐条套餐配额 + 逐条奖励活动各一行（奖励活动含
            //     已过期的），另加一条联网搜索（单位「次」，与文档级的「积分」不同）。
            //   `credit_expiry` —— 上面那些行都带 `expireAt`（毫秒，`expire_ms > 0`
            //     才写），所以「快到期优先」排序对它可用。
            .with_account_features(proto::AccountFeatures {
                usage: true,
                credit_detail: true,
                credit_expiry: true,
                identifier: vec!["id".to_string()],
                expiry: vec!["expiresAt".to_string()],
                ..Default::default()
            })
        .with_checkin(proto::CheckinInfo {
            description: Some(
                "一次领当天全部奖励（每日签到 + 新人礼） · 幂等由上游保证".to_string(),
            ),
            modes: vec![proto::CheckinMode::Checkin],
            onboarding: true,
            ..Default::default()
        })
    }

    /// 流内拒绝：上游把**内容审核**包装成 `HTTP 200 + finish_reason=content_filter`
    /// 的假答复（正文是一句固定拒绝话术）。
    ///
    /// 声明它之后编排层会把它当**可轮换失败**：队列里还有别的账号/家就换，
    /// 只剩这一家时给客户端一句可读错误（不再把套话当答案）。
    ///
    /// ⚠️ 只声明这一个取值：`length`（推理吃光预算）是**正常**收尾，不该换家。
    fn refusal_markers(&self) -> (Vec<String>, Vec<String>) {
        (vec!["content_filter".to_string()], Vec::new())
    }

    // ── 数据面 ────────────────────────────────────────────────

    /// 构造 `POST {网关}/v2/chat/completions`。
    ///
    /// 上游恒流式（见 `chat` 的模块头）；非流式客户端请求由编排层聚合。
    fn build_chat<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ChatRequest,
    ) -> SendFuture<'a, ChatPlan> {
        Box::pin(async move {
            let record = crate::store::record(ctx, ID, &req.account).await?;
            let credential = credentials::from_record(Some(&record))?;
            let plan = chat::build_chat_plan(&credential, &req.body)
                .map_err(|error| GatewayError::with_status(400, error).into_message())?;
            // `Headers` 在这里没有消费方（本家不透传任何客户端头），但保留
            // 转换点：将来要加上会话关联头时只改这里。
            let _client = Headers::from_map(&req.headers);
            Ok(ChatPlan::chat(plan.url, plan.headers, plan.body))
        })
    }

    /// 上游错误分类。**判据**取自实测码表，**动作**按四档语义走：
    ///
    ///   - `81113` / `exceeds the maximum size` → `Fatal`，**状态码折成 413**：
    ///     上游给这条带的是 HTTP 429，不先判掉就会被下面的限额档吞掉
    ///   - `81111` / `81112` / `81114` / `0308` / `TPM` / `rate limit` / `too many
    ///     requests` / HTTP 429 → `QuotaLimited`（冷却「该凭据 + 该模型」）
    ///   - `81004`（这个模型没权限）/ `81009`（名字不认）→ `Fatal`，不冷却账号。
    ///     探测对权限拒绝收窄清单，对 `81009` 保留并标记推理端未部署。
    ///   - `APIG.1009` / `APIG.1001` / `APIG.1002` / 401 / 403 → `Fatal`
    ///     （凭据失效，用户需重新登录/导入；不自动冷却账号池）
    ///   - 其余 → `Fatal`
    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        let text = body.to_string();
        let lower = text.to_ascii_lowercase();
        // ── 体积超限**必须先于** `status == 429` 那一条判掉 ──────────────
        // 上游给这条的 HTTP 码就是 429。落进限额档的后果有两层：① 账号被记一次
        // 限额冷却并换号 —— 可它拒的是这份 body 的条数，跟这个号无关，换号后同一份
        // body 照样被拒；② 客户端收到透传的 429，按「暂时忙」继续无限重试一个
        // **必然失败**的请求。上游原文自报了家门（`messages max size is: 5000`）。
        if request_is_too_large(&lower) {
            return proto::ErrorClass::Fatal {
                status: 413,
                message: format!(
                    "请求超过上游上限：{text}（这条拒的是 messages 的条数，上游实测上限 5000 条，\
                     与 token 数无关：把历史清短一些 —— 要减少条数，光把每条改短没用）"
                ),
                upstream_code: None,
            };
        }
        let rate_limited = status == 429
            || ["81111", "81112", "81114", "0308", "tpm", "rate limit", "too many requests"]
                .iter()
                .any(|marker| lower.contains(marker) || text.contains(marker));
        if rate_limited {
            return proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message: format!("上游返回 {status}: {text}"),
                upstream_code: None,
                status,
            };
        }
        proto::ErrorClass::Fatal {
            status,
            message: format!("上游返回 {status}: {text}"),
            upstream_code: None,
        }
    }

    // ── 凭证 ──────────────────────────────────────────────────

    /// 取转发凭证：**只做存在性校验**（与 CatPaw 同一口径 —— 它也没有刷新机制）。
    ///
    /// 转发凭证是不过期的网关 Basic 对，没有「一个可刷新的 token」这东西，所以
    /// 这条必然是空操作 ⇒ 回 `Ok` + 空串。调用方只看成/败。
    ///
    /// ⚠️ 不要改回「恒回一个 401」：编排层其实**每次出站前**都问一次（不只是 401
    /// 之后），那样每发一次请求就多一条「凭证准备失败」的噪声日志。
    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let record = crate::store::record(ctx, ID, account).await?;
            // 校验「这对网关凭据齐不齐」—— 齐了就没有任何需要准备的东西
            credentials::from_record(Some(&record))?;
            Ok(String::new())
        })
    }

    /// 控制面续期：refresh token + DPoP 私钥 → 新的临时 AK/SK，写回账号记录。
    ///
    /// 返回新的 `accessKeyId`（协议契约要一个非空串表成功）。
    ///
    /// ── 为什么续期必须在插件里 ──────────────────────────────────
    /// 续期要 `oauth::refresh`：它用登录时那把 **DPoP 私钥**签一份新 proof 去打
    /// 令牌端点。那套东西（PKCE、P-256、DPoP proof、STS 换凭据）是**上游协议
    /// 细节** —— 它留在网关，主仓库就仍然携带「怎么以官方客户端形态换凭据」的
    /// 知识，而那正是这次拆分要消掉的东西。
    ///
    /// 一次性 refresh token **每轮轮换**：同账号刷新串行执行并回读凭据，
    /// 写回仍走「比较再写」。
    fn refresh_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let credential = control::refresh(ctx, account, None)
                .await
                .map_err(GatewayError::into_message)?;
            Ok(credential.access_key_id)
        })
    }

    /// 该账号的控制面凭据是否临期（到期前 30 分钟算临期）。
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
            Ok(control::credentials_expiring(&record))
        })
    }

    // ── 目录 ──────────────────────────────────────────────────

    /// 当前模型清单：权限拒绝隐藏，推理端未部署的条目保留并标记。
    fn models<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async move {
            let mut list = models::list();
            probe::apply(&mut list);
            Ok(proto::ModelList {
                models: list,
                meta: proto::CatalogMeta {
                    remote: models::remote_refreshed(),
                    refreshed_at: models::last_refreshed_at(),
                },
            })
        })
    }

    /// 刷新模型目录（`GET {网关}/v1/models`，只用网关 Basic 凭据）。
    ///
    /// 刷新成功后 `models::refresh` 会顺手起一轮可用性探测（6 小时 TTL +
    /// 在途标记，不会因为目录被反复拉而反复打上游）。
    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            let record = match crate::store::record(ctx, ID, account).await {
                Ok(record) => Some(record),
                // 空 account = 库里有几条用第一条；一条都没有时才走到这里
                Err(_) if account.is_empty() => None,
                Err(error) => {
                    return Ok(proto::RefreshOutcome::failed(error.message));
                }
            };
            if record.is_none() && !account.is_empty() {
                return Ok(proto::RefreshOutcome::failed(
                    "指定的账号不存在或不可用，请重新选择",
                ));
            }
            Ok(models::refresh_from_record(ctx, record.as_ref(), force).await)
        })
    }

    // ── 余额 / 签到 / 领取 ─────────────────────────────────────

    /// 查余额 / 积分：读订阅快照（`GET /v1/subscription`，V11 签名）。
    ///
    /// 控制面凭据缺失时回 `usage_not_configured`（400 + 中性标记）—— 手工只导入
    /// 网关 Basic 凭据的账号会走到这条，前端显示成「未配置查询」而不是失败。
    fn quota_sources(&self, report: &Value) -> Vec<Value> {
        crate::quota::sources(&self.meta().id, report)
    }

    fn usage<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, Value> {
        Box::pin(async move {
            balance::query_usage(ctx, account)
                .await
                .map_err(GatewayError::into_message)
        })
    }

    /// 每日签到：`POST /v1/subscription/bonus/claim` 领奖励积分。
    ///
    /// 上游**一次调用**把当天所有可领的奖励（每日签到 + 一次性新人礼）一起发
    /// 下来，所以签到与新手任务是同一个动作。幂等由上游保证（重复领不报错）。
    fn checkin<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(async move {
            let claim = checkin::claim_daily_checkin(ctx, account)
                .await
                .map_err(GatewayError::into_message)?;
            Ok(checkin_report_of(&claim))
        })
    }

    /// 新手任务状态（与签到同一个上游动作的只读视图：列订阅里的奖励活动）。
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

    /// 领取全部未领的奖励（与每日签到同一个上游动作）。
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

    /// 添加账号：手工导入网关凭据（`baseUrl` + `modelAppKey` / `modelAppSecret`）。
    ///
    /// 自助 OAuth 走 [`Self::login_start`] 那条路（轮询型）；这里只处理
    /// 「用户已经从别的机器导出过凭据」这一种形态。
    fn normalize_account<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move { normalize(input).await.map_err(GatewayError::into_message) })
    }

    fn account_view(&self, secret: &Value) -> Result<Value, String> {
        Ok(account_view(secret))
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(identity_of(secret))
    }

    // ── 登录（轮询型）──────────────────────────────────────────

    /// 发起网页登录：向云端要 state、现造 PKCE + DPoP，拼出授权地址。
    ///
    /// ── 为什么 `poll_interval_ms` 必须大于 0 ──────────────────
    /// 授权回调在**云端**（`/v1/claw/auth/callback`）终结，永远不回到本机 ——
    /// 桌面端就是靠轮询取回授权码的。报成 0 会让网关去等一个不存在的回调，
    /// 症状是「地址出来了、界面一直挂着」。
    fn login_start<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        method: &'a str,
        _payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(async move {
            if method != "browser" && !method.is_empty() {
                return Err(format!("OfficeAce 不支持这种登录方式：{method}"));
            }
            let (flow, url, interval) = oauth::start().await?;
            Ok(proto::LoginStart {
                flow,
                url,
                instructions:
                    "在打开的页面里完成华为云登录；完成后回到这里，网关会自动取回凭证"
                        .to_string(),
                poll_interval_ms: interval,
                // 华为云登录页在系统浏览器里更稳（内嵌窗口里第三方登录常被拒）
                external_browser: true,
                allowed_hosts: vec!["auth.huaweicloud.com".to_string()],
            })
        })
    }

    /// 轮询一次「用户授权完了没」，完成时把凭据换成 `NormalizedAccount`。
    fn login_poll<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async move {
            match oauth::poll(flow).await {
                Ok(None) => Ok(proto::LoginPoll::Pending { hint: None }),
                Ok(Some(secret)) => Ok(proto::LoginPoll::Done {
                    account: account_of(secret),
                }),
                Err(error) => Ok(proto::LoginPoll::Failed {
                    message: error.message,
                }),
            }
        })
    }

    /// 取消一轮登录（把进程内的待办删掉）。
    fn login_cancel<'a>(&'a self, _ctx: &'a PluginCtx, flow: &'a str) -> SendFuture<'a, ()> {
        Box::pin(async move {
            oauth::cancel(flow);
            Ok(())
        })
    }
}

/// 登录换来的 secret → 落库形态的 `NormalizedAccount`。
///
/// ── 账号名怎么定 ────────────────────────────────────────────
/// 上游显示名 → 用户级 principal id → 账号级 account id → 派生 id 的前 8 位。
/// **不退到种子名「OfficeAce 果办」** —— 多个账号会同名而分不出来。
/// principal 排在 account 之前：同一华为云账号下的不同 IAM 用户共用 account_id，
/// 那串十六进制分不开他们。
fn account_of(secret: Value) -> proto::NormalizedAccount {
    let identity = identity_of(&secret);
    let app_key = crate::store::pick(&secret, &["modelAppKey", "appKey"]);
    let tail: String = identity
        .chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let name = {
        let upstream = crate::store::pick(&secret, &["accountName", "principalId", "accountId"]);
        if !upstream.is_empty() {
            upstream
        } else if app_key.is_empty() {
            "OfficeAce 果办".to_string()
        } else {
            format!("OfficeAce {tail}")
        }
    };
    let mut secret = secret;
    if let Some(object) = secret.as_object_mut() {
        object.insert("name".to_string(), Value::String(name));
    }
    proto::NormalizedAccount {
        public: account_view(&secret),
        secret,
        identity,
        desktop: false,
    }
}

// ─── 上游「请求太大」的判据 ────────────────────────────────────

/// 上游「请求太大」的判据（`classify` 用它把超限折成可读的 413）。
///
/// 取值集合抄自实测：`81113` 是条数超限那条线给的码，其余几条是同一件事在
/// APIG / MaaS 两层的不同措辞。参数**要已小写**。
fn request_is_too_large(lower: &str) -> bool {
    [
        "81113",
        "exceeds the maximum size",
        "requestentitytoolarge",
        "payload too large",
        "request body is too large",
        "request body is too long",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

// ─── 账号记录 / 视图 ───────────────────────────────────────────

/// 取一条账号记录（网关记录形态：secret 摊平 + `id` / `provider` / `name`）。
///
/// 非空 account 必须命中，否则报「不存在或不可用」—— 与网关
/// `store.officeace_account_record().ok_or_else(...)` 同一文案。
async fn record_of(ctx: &PluginCtx, account_id: &str) -> Result<Value, GatewayError> {
    match crate::store::record(ctx, ID, account_id).await {
        Ok(record) => Ok(record),
        Err(_) => Err(GatewayError::with_status(
            400,
            "OfficeAce 账号不存在或不可用，请重新选择",
        )),
    }
}

/// 账号公开视图（界面账号页读的那几个键）。
///
/// ── 为什么把「有没有控制面凭据」显式给出来 ──────────────────
/// 手工只导入网关 Basic 那对时，转发完全可用但额度/签到查不了。界面按
/// `hasControlPlane` 决定「积分」按钮是否可点（点了会回 `usage_not_configured`，
/// 那是个中性提示，不是失败）。
fn account_view(secret: &Value) -> Value {
    let app_key = crate::store::pick(secret, &["modelAppKey", "appKey", "model_app_key"]);
    let base_url = crate::store::pick(secret, &["baseUrl", "base_url", "api_base", "apiBase"]);
    let has_control = !crate::store::pick(
        secret,
        &["accessKeyId", "access_key_id", "accessKey"],
    )
    .is_empty();
    let email = crate::store::pick(secret, &["email"]);
    let name = {
        let explicit = crate::store::pick(secret, &["name"]);
        if !explicit.is_empty() {
            explicit
        } else if !email.is_empty() {
            email
        } else if !app_key.is_empty() {
            // 网关凭据的前 6 位足以让用户认出是哪一个 app（完整 key 不落界面）
            format!("OfficeAce {}", &app_key.chars().take(6).collect::<String>())
        } else {
            "OfficeAce 账号".to_string()
        }
    };
    serde_json::json!({
        "name": name,
        "baseUrl": base_url,
        "hasControlPlane": has_control,
        "hasSession": true,
        "canRefresh": has_control,
    })
}

/// 账号的稳定身份（去重用）：`officeace-<hash>`。
///
/// ── 为什么不是 `app-<modelAppKey>` ───────────────────────────
/// 这个 id 是**落盘契约**：内置版时代落库的账号 id 就是 `officeace-<hash>`
/// （hash 取「规范化基址 + app key」的 FNV-1a 64 位前 12 位）。插件若换一套
/// 派生规则，升级后重新导入同一套凭据会**多出一条重复账号**，而老账号还占着
/// 队列位置。所以这里逐字复刻内置版的算法（含基址的 `/v2` 规范化）。
fn identity_of(secret: &Value) -> String {
    let app_key = crate::store::pick(secret, &["modelAppKey", "appKey", "model_app_key"]);
    if app_key.is_empty() {
        return String::new();
    }
    let base_url = crate::store::pick(secret, &["baseUrl", "base_url", "api_base", "apiBase"]);
    format!(
        "officeace-{}",
        short_hash(&format!(
            "{}|{}",
            chat::normalize_base(&base_url),
            app_key
        ))
    )
}

/// FNV-1a 64 位（`&str` → 16 位 hex，取前 12 位）。与内置版逐字一致。
fn short_hash(input: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")[..12].to_string()
}

// ─── 账号规范化 ────────────────────────────────────────────────

/// 手工导入路径：`{baseUrl, modelAppKey|appKey, modelAppSecret|appSecret}`。
///
/// 兼容两种键名（本仓落盘名 与 桌面端 `models.json` 的写法），由
/// `credentials::from_record` 的候选表统一处理 —— 这里只做「有没有齐」的判定。
async fn normalize(input: proto::NormalizeInput) -> Result<proto::NormalizedAccount, GatewayError> {
    let payload = match input.source {
        proto::NormalizeSource::Manual { payload } => payload,
        proto::NormalizeSource::FileText { content, .. } => {
            serde_json::from_str::<Value>(content.trim()).map_err(|_| {
                GatewayError::with_status(
                    400,
                    "不是合法 JSON（请选择一份包含 baseUrl 与 modelAppKey / modelAppSecret 的文件）",
                )
            })?
        }
        proto::NormalizeSource::FileBase64 { .. } => {
            return Err(GatewayError::with_status(
                400,
                "OfficeAce 不支持二进制凭证文件，请粘贴 API 基址与网关凭据",
            ))
        }
        proto::NormalizeSource::Desktop => {
            return Err(GatewayError::with_status(
                400,
                "OfficeAce 不支持导入本机登录态（客户端不落可复用的登录文件），\
                 请粘贴 API 基址与网关凭据",
            ))
        }
    };
    // 唯一硬校验：网关 Basic 那一对必须齐（这是转发的前提）
    let credential = credentials::from_record(Some(&payload))?;
    let mut secret = serde_json::json!({
        "baseUrl": credential.base_url,
        "modelAppKey": credential.model_app_key,
        "modelAppSecret": credential.model_app_secret,
    });
    // 控制面那一半可选（手工导入时通常没有）
    for (key, value) in [
        ("accessKeyId", credential.access_key_id),
        ("secretAccessKey", credential.secret_access_key),
        ("securityToken", credential.security_token),
        ("projectId", credential.project_id),
    ] {
        if !value.is_empty() {
            secret[key] = Value::String(value);
        }
    }
    if credential.expires_at > 0 {
        secret["expiresAt"] = Value::from(credential.expires_at);
    }
    if let Some(name) = input
        .name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        secret["name"] = Value::String(name.to_string());
    }
    Ok(proto::NormalizedAccount {
        public: account_view(&secret),
        identity: identity_of(&secret),
        secret,
        desktop: false,
    })
}

// ─── 签到结果 → 协议档位 ──────────────────────────────────────

/// 把 `{success, msg, gained}` 翻成协议档位。
///
/// 本家的 `success: true` 覆盖两种情形（真的领到 / 今天已领过），所以 `gained`
/// 为 0 时要归 `Done` 而不是 `Claimed` —— 那决定调度链要不要记一笔「今天签到了」。
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
    let gained = claim
        .get("gained")
        .and_then(Value::as_f64)
        .filter(|points| *points > 0.0);
    let credit = gained.map(|points| format!("{points:.0} 积分"));
    // 没有增量读数（领取前那次查询失败）时按 `success` 判：上游说成了就是成了
    let outcome = if !success {
        proto::CheckinOutcome::Failed
    } else if gained.is_some() || claim.get("gained").is_none() {
        proto::CheckinOutcome::Claimed
    } else {
        // `gained: 0` = 今天没有可领的（上游已结算过）
        proto::CheckinOutcome::Done
    };
    proto::CheckinReport {
        outcome,
        credit,
        streak: None,
        message,
    }
}
