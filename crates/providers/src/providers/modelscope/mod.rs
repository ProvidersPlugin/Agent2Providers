//! ModelScope（魔搭）插件实现：**无状态 OpenAI 兼容转发 + 每日签到 + 魔粒余额**。
//!
//! ── 从网关搬来时改了什么（逐条对照，便于 parity 核对）────────────
//! ```text
//!   不变：两套域名、`ms-` 令牌两用、include_usage 注入、错误分类的分支顺序、
//!         账号相关目录、10 分钟 TTL、探活即签到、魔粒 + 今日用量两读数
//!   改：  ① 凭证从 `store.modelscope_account_record()` 改成 `ctx.store_get()`；
//!         ② 目录缓存不落盘（网关自己会缓存插件返回的清单）；
//!         ③ 账号 id 由插件生成（`modelscope-<HavanaId>` / `modelscope-intl-…`），
//!            网关按 `identity` 去重；
//!         ④ `Region::kind()` 与 `kind_id` 消失 —— 插件里只有字符串 id。
//! ```
//!
//! ── 三个实测事实决定了本家的形状 ─────────────────────────────
//!   1. **一个 `ms-` 令牌两用**（Bearer 调推理、cookie 值调网页站）⇒ 只要一个
//!      凭证字段；
//!   2. **两个站点的账号体系是分开的** ⇒ 地区必须是 provider 身份而不是账号上
//!      的字段；
//!   3. **模型清单是账号相关的**（实测 36 条，不是全站几千个）⇒ 不编静态兜底表。
//!
//! ── 思考等级 ────────────────────────────────────────────────
//! 上游是标准 OpenAI 兼容，`/v1/models` 不给能力位（实测只有 id），因此
//! **不做** `reasoning_patch`：按 id 猜思考位会在两个方向上一起猜错。
//!
//! ── 一处**必须**注入的字段（会静默毁账的坑）────────────────────
//! 见 [`request_body`] 的说明：不注入 `stream_options.include_usage` 时，
//! 魔搭的流式响应**永远不给真值 usage**（每个 chunk 都带全零），网关的记账
//! 与客户端的 token 显示会一起归零。

pub mod balance;
pub mod checkin;
pub mod client;
pub mod credentials;
pub mod errors;
pub mod models;
pub mod probe;
pub mod region;
mod transactions;
mod image_generation;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ChatPlan, PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::Value;

pub use region::Region;

/// 按地区参数化的适配器：**一份实现、两个实例**。
///
/// 地区不是账号字段而是 provider 身份，因此两个实例各自申报自己的 id ——
/// 这样「这条账号打哪个站」由身份保证，不需要在每一处转发/查询里再读一次
/// 账号上的地区。
pub struct ModelScopeProvider {
    region: Region,
}

impl ModelScopeProvider {
    /// 国内站
    pub const CN: ModelScopeProvider = ModelScopeProvider { region: Region::Cn };
    /// 国际站
    pub const INTL: ModelScopeProvider = ModelScopeProvider { region: Region::Intl };
}

/// 出站请求体：原样透传 + 把 `stream_options.include_usage` 置真。
///
/// ── 为什么必须注入（实测，一个会静默毁账的坑）───────────────────
/// 魔搭的流式响应是**非标准形态**：**每一个 chunk 都带一个 `usage` 对象**，
/// 而且**值全是 0**（标准 OpenAI 是除最后一块外都为 `null`）。真值只在末尾
/// 那个 `choices: []` 的 usage-only 块里 —— **而且那一块只在请求显式带上
/// `stream_options.include_usage: true` 时才发**。实测对照：
///
/// ```text
///   不带 include_usage：最后一帧是 {"choices":[{"finish_reason":"stop"}],
///                                  "usage":{0,0,0}}          ← 真值根本没出现
///   带   include_usage：{"choices":[], "usage":{62,32,94}}    ← 真值在这一块
/// ```
///
/// 后果链：本网关**恒以流式请求上游**，客户端要非流式时再由聚合器把流折成
/// 完整响应。不注入这个开关时，聚合器读到的最后一个 usage 就是那串全零 ——
/// 于是**非流式请求的 usage 恒为 0**，请求日志、报表与前端 token 显示一起归零，
/// 而内容完全正常（这个故障只在统计上可见，排查时极难联想到「上游少发了一帧」）。
///
/// 客户端已经自己写了 `stream_options` 时**保留它的其余键**、只把
/// `include_usage` 置真：覆盖整个对象会把用户显式设过的东西悄悄丢掉。
///
/// 非对象 body（畸形请求）原样返回，不在这一层报错 —— 让上游去拒绝它。
fn request_body(body: &Value) -> Value {
    let mut payload = body.clone();
    let Some(object) = payload.as_object_mut() else {
        return payload;
    };
    let mut options = object
        .get("stream_options")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    options.insert("include_usage".to_string(), Value::Bool(true));
    object.insert("stream_options".to_string(), Value::Object(options));
    payload
}

impl Provider for ModelScopeProvider {
    fn meta(&self) -> ProviderMeta {
        let region = self.region;
        ProviderMeta::new(region.provider_id(), format!("ModelScope {}", region.label()))
            .with_capabilities(proto::Capabilities {
                chat: true,
                // 标准 OpenAI 协议，一次请求一次回答
                stateful: false,
                // 令牌长期有效、上游没有 refresh 接口
                refresh: false,
                model_refresh: true,
                // 魔粒钱包
                usage: true,
                // 每日签到（读登录信息即触发）
                checkin: true,
                claims: false,
                // 粘贴式令牌，不开窗口
                web_login: false,
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                refresh_uses_account: false,
                // 本家是否认「默认模型」概念（协议新增位，见 `Capabilities`）
                supports_default_model: false,
                // 本家有没有环境变量 / 默认登录态旁路（协议新增位）
                env_credentials: false,
            })
            .with_login(vec![proto::LoginMethod::Token {
                label: "ms- 令牌".into(),
                placeholder: "粘贴 ms- 开头的令牌".into(),
                extra_fields: vec![],
            }])
            // 本家没有内置兜底清单（清单是账号相关的，见 `models.rs`）
            .with_models(vec![])
            // 账号行：两个站点同一份申报（账号字段、余额形态、签到链路两地
            // 完全一致，差别只在域名）。
            //   `usage` —— 额度是魔粒钱包（`available_balance`），外加「今日已用
            //     调用数」一行；`credit_detail` 挂出余额详情，三条 snapshot 读数
            //     不作为积分包计算耗尽、到期或剩余比例。
            //   `edition: false` —— 地区由 provider id 直接表达（两家 provider、
            //     两个分组），不需要再渲染一个地区列。
            //   `identifier: userId` —— 身份键存的是上游的 HavanaId。
            //   `expiry` 空 + `long_lived: true` —— `ms-` 令牌是长期有效的 API
            //     key（插件源码原话「session 就是 api key 本身……永不过期」，
            //     实测也没有任何 refresh / renew 接口），没有到期时刻可算。
            .with_account_features(proto::AccountFeatures {
                usage: true,
                credit_detail: true,
                identifier: vec!["userId".to_string()],
                long_lived: true,
                ..Default::default()
            })
        .with_checkin(proto::CheckinInfo {
            description: Some(match region {
                Region::Cn => "无独立签到接口 · 每天替账号打一次登录信息（即当日魔粒）".to_string(),
                Region::Intl => "与国内版同一套链路 · 站点不同、令牌不通用".to_string(),
            }),
            modes: vec![proto::CheckinMode::Checkin],
            ..Default::default()
        })
    }

    fn provider_id(&self) -> String {
        self.region.provider_id().to_string()
    }

    // ── 无状态转发 ────────────────────────────────────────────

    /// 构造 `POST {推理域}/chat/completions`（标准 OpenAI 协议，body 原样透传
    /// **外加一个 `stream_options.include_usage`**，理由见 [`request_body`]）。
    ///
    /// ── 为什么**不**改别的（模型与流式意向都原样透传）────────────────
    /// 魔搭的 `/v1/models` 里混着非对话模型，但「哪些模型支持什么」本插件没有
    /// 任何可信依据去猜（按 id 猜会猜错），让上游自己判 —— 它给的 400 会被
    /// `classify` 如实透出。模型 id 也原样发（`组织/模型名`）。
    fn build_chat<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ChatRequest,
    ) -> SendFuture<'a, ChatPlan> {
        Box::pin(async move {
            let region = self.region;
            let found = ctx
                .store_get(region.provider_id(), &req.account)
                .await
                .map_err(|error| format!("读取 ModelScope 账号失败：{error}"))?;
            let token = credentials::token_of(&found.secret)?;
            let headers = vec![
                // 网关的发送层会自动补 Content-Type；这里只加鉴权与 Accept
                ("Authorization".to_string(), format!("Bearer {token}")),
                ("Accept".to_string(), "text/event-stream".to_string()),
            ];
            Ok(ChatPlan::chat(
                format!("{}/chat/completions", region.inference_base_url()),
                headers,
                request_body(&req.body),
            ))
        })
    }

    /// 上游错误分类。**顺序要紧**：两条「账号级」判据必须在裸 401/403 与裸
    /// 429 之前，否则它们会被归成「令牌失效」或「速率窗口」，各自的正确处置
    /// 就都丢了。
    ///
    ///   1. **魔粒耗尽**（429 + `insufficient balance`）→ `QuotaLimited` +
    ///      冷却到下一个北京零点。它不是速率窗口：充值之前这把令牌**一直**
    ///      不可用，按分钟冷却只会在当天反复白撞；
    ///   2. **未验证邮箱**（403 + 特征串）→ 同样冷却到次日。⚠️ 漏了这条的
    ///      后果最糟：403 会被当成「令牌失效」，界面提示用户去**重新粘贴一个
    ///      本来就好好的令牌**，而真正要做的是去账号设置页完成邮箱验证；
    ///   3. **裸 401 / 403** → `TokenExpired`；
    ///   4. **其余 429** → `QuotaLimited`，不给结构化恢复时间（`reset_at: None`，
    ///      由网关的限额标记给 10 分钟兜底）；
    ///   5. 其余 → 内容拦截分类（审核文案不罚账号）或原样透传。
    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        let raw = body
            .get("message")
            .or_else(|| body.get("Message"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误");
        let upstream_code = body
            .get("code")
            .or_else(|| body.get("Code"))
            .and_then(Value::as_i64);
        let message = format!("上游返回 {status}: {raw}");
        let label = self.region.label();

        if errors::is_insufficient_balance(status, raw) {
            return proto::ErrorClass::QuotaLimited {
                reset_at: Some(errors::next_day_boundary_ms(crate::http::now_ms())),
                message: format!(
                    "ModelScope {label}账号的魔粒已耗尽（上游：{raw}），将在下一个自然日\
                     （北京时间 0 点）自动恢复；给账号充值后立即恢复"
                ),
                upstream_code,
                status,
            };
        }
        if errors::is_unverified_account(status, raw) {
            return proto::ErrorClass::QuotaLimited {
                reset_at: Some(errors::next_day_boundary_ms(crate::http::now_ms())),
                message: format!(
                    "ModelScope {label}账号未完成邮箱验证，魔粒推理接口拒绝服务\
                     （上游：{raw}）——请到魔搭账号设置页完成邮箱验证，验证后次日自动恢复"
                ),
                upstream_code,
                status,
            };
        }
        if status == 401 || status == 403 {
            return proto::ErrorClass::TokenExpired { message };
        }
        if status == 429 {
            return proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code,
                status,
            };
        }
        content_or_fatal(status, body, message, upstream_code)
    }

    fn supports_image_generation(&self) -> bool { true }
    fn supports_image_editing(&self) -> bool { true }

    fn build_image<'a>(&'a self, ctx: &'a PluginCtx, req: agent2api_provider_sdk::images::ImageRequest) -> SendFuture<'a, agent2api_provider_sdk::images::ImagePlan> {
        Box::pin(image_generation::build(ctx, req, self.region, false))
    }

    fn build_image_edit<'a>(&'a self, ctx: &'a PluginCtx, req: agent2api_provider_sdk::images::ImageRequest) -> SendFuture<'a, agent2api_provider_sdk::images::ImagePlan> {
        Box::pin(image_generation::build(ctx, req, self.region, true))
    }

    // ── 凭证 ──────────────────────────────────────────────────

    /// 取可用令牌。本家**没有续期**：这里只做「记录里有没有令牌」的检查，
    /// 服务端拒了就是 401，网关会把它归成 `TokenExpired` 并在刷新失败后
    /// 如实报给客户端（「请重新粘贴令牌」）。
    fn ensure_token<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, String> {
        Box::pin(async move {
            let region = self.region;
            let found = ctx
                .store_get(region.provider_id(), account)
                .await
                .map_err(|error| format!("读取 ModelScope 账号失败：{error}"))?;
            credentials::token_of(&found.secret)
        })
    }

    // ── 目录 ──────────────────────────────────────────────────

    fn models<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async move { Ok(models::list(self.region)) })
    }

    /// 刷模型目录：用指定账号（空串 = 本家第一个可用账号）的令牌。
    ///
    /// 「一个账号都没有」不是失败 —— 那是「用户还没添加账号」的正常状态，
    /// 返回 `unchanged()`（与网关版同口径：脚本 / CI 不该看到一个红色失败）。
    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            let region = self.region;
            let api = match ctx.store_get(region.provider_id(), account).await {
                Ok(found) => match client::ControlClient::from_store(ctx, region, &found) {
                    Ok(api) => api,
                    Err(error) => return Ok(proto::RefreshOutcome::failed(error)),
                },
                Err(agent2api_provider_sdk::StoreError::NotFound) if account.is_empty() => {
                    return Ok(proto::RefreshOutcome::unchanged());
                }
                Err(error) => {
                    return Ok(proto::RefreshOutcome::failed(format!("读取 ModelScope 账号失败：{error}")));
                }
            };
            Ok(models::refresh(region, &api, force).await)
        })
    }

    // ── 余额 / 签到 ───────────────────────────────────────────

    /// 查魔粒余额与今日已用调用数（见 `balance.rs`）。
    fn quota_sources(&self, report: &Value) -> Vec<Value> {
        crate::quota::sources(&self.meta().id, report)
    }

    fn usage<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::UsageReport> {
        Box::pin(async move {
            let api = client::ControlClient::for_account(ctx, self.region, account).await?;
            balance::query_usage(&api).await
        })
    }

    /// 每日签到（读登录信息即触发，见 `checkin.rs`）。
    fn checkin<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(async move {
            let api = client::ControlClient::for_account(ctx, self.region, account).await?;
            checkin::claim(&api).await
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    /// 规范化一条账号：取令牌 → **出网探活（同时签到）** → 用 `HavanaId` 做身份。
    fn normalize_account<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move {
            let region = self.region;
            let (token, name) = match &input.source {
                proto::NormalizeSource::Manual { payload } => {
                    let token = credentials::token_from_payload(payload);
                    let name = payload
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string);
                    (token, name)
                }
                proto::NormalizeSource::FileText { content, .. } => {
                    let trimmed = content.trim();
                    if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
                        let token = credentials::token_from_payload(&parsed);
                        (token, None)
                    } else {
                        (trimmed.to_string(), None)
                    }
                }
                proto::NormalizeSource::FileBase64 { .. } => {
                    return Err("ModelScope 不支持二进制文件导入，请粘贴 ms- 令牌".to_string());
                }
                proto::NormalizeSource::Desktop => {
                    return Err(
                        "ModelScope 不支持导入桌面端登录态，请粘贴 ms- 令牌添加账号".to_string()
                    );
                }
            };
            if token.is_empty() {
                return Err("缺少令牌（accessToken / apiKey / token / session）".to_string());
            }
            let api = client::ControlClient::new(region, token.clone(), input.proxy.as_ref(), &ctx.env.proxy)?;
            let identity = probe::identify(&api, region).await?;
            let user_id = identity.user_id.trim().to_string();
            let id = format!("{}{user_id}", region.account_id_prefix());
            let display = input
                .name
                .clone()
                .or(name)
                .or_else(|| {
                    (!identity.display_name.is_empty()).then(|| identity.display_name.clone())
                })
                .unwrap_or_else(|| {
                    format!("ModelScope {} {}", region.label(), credentials::token_tail(&token))
                });
            Ok(proto::NormalizedAccount {
                secret: serde_json::json!({
                    "accessToken": token,
                    "userId": user_id,
                }),
                public: serde_json::json!({
                    "name": display,
                    "uid": user_id,
                    "tokenTail": credentials::token_tail(&token),
                    "edition": region.label(),
                }),
                identity: id,
                desktop: false,
            })
        })
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        let user_id = secret
            .get("userId")
            .or_else(|| secret.get("havanaId"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if user_id.is_empty() {
            // 没有身份时用令牌尾号兜底（会退化成按凭证去重，网关会提示）
            let token = credentials::token_of(secret)?;
            return Ok(format!(
                "{}{}",
                self.region.account_id_prefix(),
                credentials::token_tail(&token)
            ));
        }
        Ok(format!("{}{user_id}", self.region.account_id_prefix()))
    }
}

/// 内容拦截的文案特征（全部小写；匹配前把上游文案转小写）
const CONTENT_BLOCK_PATTERNS: &[&str] = &[
    "blocked by security policy",
    "unapproved channel",
    "illegal api invocation",
];

/// 内容拦截的提示文案
const CONTENT_BLOCK_HINT: &str = "；内容被上游内容策略拒绝（这类拦截按逐字匹配，不是账号问题）：\
若为客户端 system 模板误报，可在设置页「通用 → 系统提示词」切到「替换」模式；\
若为对话内容本身触发审核，请调整后重试";

/// 命中内容拦截文案 → `ContentBlocked`（并补提示），否则 → `Fatal`
fn content_or_fatal(
    status: u16,
    body: &Value,
    message: String,
    code: Option<i64>,
) -> proto::ErrorClass {
    let matched = status >= 400
        && body
            .get("message")
            .and_then(Value::as_str)
            .map(|text| {
                let lowered = text.to_lowercase();
                CONTENT_BLOCK_PATTERNS
                    .iter()
                    .any(|pattern| lowered.contains(pattern))
            })
            .unwrap_or(false);
    if matched {
        proto::ErrorClass::ContentBlocked {
            status,
            message: format!("{message}{CONTENT_BLOCK_HINT}"),
            upstream_code: code,
        }
    } else {
        proto::ErrorClass::Fatal {
            status,
            message,
            upstream_code: code,
        }
    }
}
