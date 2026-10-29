//! Qoder 提供商（两个地区共用一套实现，按 `Region` 参数化）。
//!
//! ── 上游长什么样（移植来源 `Qoder-Proxy`）─────────────────────
//!   - 推理网关：`POST {gateway}algo/api/v2/service/pro/sse/agent_chat_generation`
//!     （`FetchKeys=llm_model_result&AgentId=agent_common&Encode=1`）。
//!     鉴权**不是 Bearer 令牌**，而是一套自签名的 COSY 头（见 `cosy.rs`）；
//!     请求体也不是裸 JSON，要先做一遍字节编码（`cosy::encode_body`），
//!     而签名覆盖**编码之后**的字节。
//!   - 模型目录：`GET {gateway}algo/api/v2/model/list?Encode=1`（同一套签名）。
//!   - 响应：SSE，但**多包了一层信封** —— `{statusCodeValue, body}`，
//!     真正的 OpenAI chunk 是 `body` 里的**内层 JSON 字符串**。
//!     业务错误也走这一层（HTTP 永远是 200，错误在 `statusCodeValue` 里）。
//!   - 地区：国际版 `api3.qoder.sh` / 中国版 `gateway.qoder.com.cn`，
//!     两边的模型目录与凭证**不通用**（见 `endpoints::Region`）。
//!
//! ── 为什么走会话式转发（`stateful`）──────────────────────────
//! 无状态路径假设「请求体由通用层序列化后原样发出、响应是 OpenAI 协议的 SSE」。
//! Qoder 两条都不满足：请求体必须先编码再签名（通用层序列化出来的字节既没编码
//! 也没被签名覆盖），响应还要拆一层信封。因此这里实现 `conversation`，把
//! 「构造 → 发送 → 翻译」整条链收进来。
//!
//! ── 与网关那份的差别（迁移时改的）───────────────────────────
//! | 网关 | 插件 |
//! |---|---|
//! | `ProviderAdapter` + `AccountStore` | `Provider` + `PluginCtx` |
//! | `ForwardOutcome::Stream/Completion` | `ConversationSink`（head + chunk） |
//! | `RequestTelemetry` 抓包 / 用量 / 取消 | **空实现**（见 `telemetry` 模块头） |
//! | `catalog_cache` 落盘缓存 | 只留内存 |
//! | `model_rules::seed_qoder_defaults` | **不迁**（映射配置在网关侧） |
//! | 排队设置读 `config::queue_settings` | 用默认值（见 `queue` 模块头） |
//! | 账号面（`api::accounts` 的 qoder 分支） | `accounts.rs` |
//!
//! ── `stateful` 的语义在这里是什么 ──────────────────────────
//! 与 CatPaw 的「多步会话协议」不同：Qoder 的上游仍是**一次请求一次回答**，
//! 这个标记只表示「一次发送要适配器自己完成」。账号选路、限额冷却仍由编排层
//! 负责 —— 包括流内业务错误触发的换账号（见 `conversation` 的返回：额度类
//! 错误按 429 交回编排层）。
//!
//! ── 子模块 ─────────────────────────────────────────────────
//!   endpoints.rs   地区与端点（含推理网关基址）
//!   machine.rs     PKCE 随机串与本机标识（getrandom）
//!   oauth.rs       国际版 PKCE 设备授权
//!   credentials.rs 凭证格式（兼容 Qoder-Proxy 的 access/refresh）
//!   auth.rs        PAT 换取令牌 / 用户资料
//!   refresh.rs     续期（单飞 + 比较再写）
//!   balance.rs     额度查询（归一成账号页的统一形状）
//!   cosy.rs        COSY 请求签名 + 请求体编码（**推理链路的鉴权核心**）
//!   models.rs      模型目录（两地区缓存 + 静态兜底 + 远程刷新）
//!   protocol.rs    OpenAI ↔ Qoder 协议转换（消息/tools/思考档位/上游信封）
//!   context.rs     上下文档位（200K/400K/1M）：估算、选档、落到请求体
//!   errors.rs      上游错误分类（状态码 + 业务码表 + 排队态判定）
//!   stream.rs      上游 SSE 信封解包 + 思考标签拆解（跨分片）
//!   piping.rs      流式首帧预读 + 流式透传（issue #8 的换号修复在这）
//!   accounts.rs    账号面（PAT / 设备授权两种添加路径 + 归一化）
//!   checkin.rs     每日签到（中国版的活动领取；国际版没有签到计划）
//!
//! ── panic=abort ────────────────────────────────────────────
//! 本模块在对话链路上，绝不 unwrap/expect/panic。

mod accounts;
mod auth;
mod balance;
mod chat;
mod checkin;
mod context;
mod cosy;
mod credentials;
mod endpoints;
mod errors;
mod machine;
mod models;
mod oauth;
mod piping;
mod protocol;
mod refresh;
mod risk;
mod stream;

use serde_json::Value;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ConversationSink, PluginCtx, Provider, ProviderMeta, SendFuture};

use crate::content_block;
use crate::errors::GatewayError;
use crate::logging;
use crate::telemetry::RequestTelemetry;

use self::chat::Translator;
use self::endpoints::Region;
use self::stream::SseEvent;

/// 中国版实例（`qoder`）
pub static QODER_CN: QoderProvider = QoderProvider { region: Region::Cn };
/// 国际版实例（`qoder-intl`）
pub static QODER_INTL: QoderProvider = QoderProvider { region: Region::Global };

/// Qoder 提供商：**按地区参数化**（与 ZCode / Accio / AutoClaw 同款）。
///
/// 两个地区是两家 provider（`qoder` 中国版 / `qoder-intl` 国际版），地区 →
/// 身份的互查在 `endpoints::Region`。内部链路（凭证、转发、签到）仍以凭证里的
/// 地区为准 —— 拆家迁移保证了记录的 provider 与地区一致，这里的 `region`
/// 用于「队首可用账号」的选取范围与模型清单的归属。
#[derive(Clone, Copy)]
pub struct QoderProvider {
    region: Region,
}

/// 映射上绑的思考等级注入到请求体的哪个键。
///
/// `protocol::resolve_thinking` 认 `reasoning_effort` / `reasoning` / `thinking`
/// 三个键，选 `reasoning_effort` 的理由与 CatPaw 那边同：它是最标准、客户端最
/// 常传的那个，注入与「客户端已指定」在 body 上同形。
const REASONING_FIELD: &str = "reasoning_effort";

/// 一个上游响应 id（客户端按它关联同一次回答；形态照抄 OpenAI）
fn response_id() -> String {
    let uuid = cosy::random_uuid().unwrap_or_default().replace('-', "");
    let short: String = uuid.chars().take(24).collect();
    format!("chatcmpl-{short}")
}

/// 把「这个账号对这个模型已限额」记进账号库。
///
/// ── 为什么本适配器要自己做这件事（编排层不管吗）──────────────
/// 无状态路径里这个动作由编排层完成：它拿到适配器给的 `QuotaLimited` 分类，
/// 调 `rotate::mark_account_limited` 落冷却。但 Qoder 走**会话式转发**，
/// 那条路径的适配器不返回分类（「一次发送」整个在适配器内部），编排层只看得到
/// 一个 `Err`。而 Qoder 的限额信号**恰恰只出现在 SSE 信封里**（HTTP 永远是
/// 200），也就是说：不在这里落冷却，这个额度已耗尽的账号会在**每一个后续请求**
/// 上被重新选中、重新打一次上游、再失败一次 —— 界面上看不到任何冷却标记。
///
/// ── 迁移时改了什么 ────────────────────────────────────────
/// 网关那份调 `store.mark_rate_limited(...)`（同步写账号库）。插件里对应的是
/// `ctx.store_mark_limited(account, model, status, reset_at)` —— 一次 RPC，
/// 异步。因此本函数变成 async，调用点跟着 await。
async fn record_limited(
    ctx: &PluginCtx,
    account_id: &str,
    model: &str,
    _status: u16,
    kind: errors::UpstreamKind,
    message: &str,
) {
    if !matches!(kind, errors::UpstreamKind::Quota | errors::UpstreamKind::Rate) {
        return;
    }
    if account_id.is_empty() {
        return;
    }
    // reset_at 给 None：上游没在响应里给结构化恢复时间，由网关落 10 分钟兜底
    if let Err(error) = ctx
        .store_mark_limited(account_id, model, None)
        .await
    {
        logging::verbose(
            "[Qoder]",
            &format!("标记账号限额失败（不影响本次转发）：{error}"),
        );
        return;
    }
    logging::log(
        "[Qoder]",
        &format!("⚠️ 账号 {account_id} 对模型 {model} 已限额，进入冷却：{message}"),
    );
}

impl Provider for QoderProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(
            self.region.provider_id(),
            format!("Qoder {}", self.region.label()),
        )
        .with_capabilities(proto::Capabilities {
            chat: true,
            // 请求体要先编码再签名、响应要拆信封 —— 见模块头
            stateful: true,
            // 设备流 / 作业令牌两种续期路径（`refresh.rs`）
            refresh: true,
            // `GET {gateway}algo/api/v2/model/list`
            model_refresh: true,
            // 额度查询（`balance.rs`）
            usage: true,
            // 中国版有活动领取（`checkin.rs`）；国际版没有签到计划，但能力位是
            // 按 provider 申报的 —— 国际版这里也报 true，由 `checkin` 返回
            // 「无活动」而不是「不支持」（用户在界面上看得到真实原因）
            checkin: true,
            claims: false,
            // 国际版有 PKCE 设备授权（`oauth.rs`）；中国版走 PAT / 设备流
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
        .with_login(vec![
            proto::LoginMethod::Token {
                label: "个人访问令牌（PAT）".to_string(),
                placeholder: "Qoder 个人访问令牌（PAT）".to_string(),
                extra_fields: Vec::new(),
            },
            proto::LoginMethod::Browser {
                button: "网页登录".to_string(),
                hint: Some("打开 Qoder 授权页，完成后自动收下账号".to_string()),
            },
        ])
        .with_models(models::list_for(self.region))
        // 账号行：两个地区能力完全一致（拆家后是两家 provider，各自都要申报，
        // 漏一个会让国际版掉进兜底）。
        //   `credit_detail` —— 三个桶各一行（套餐额度 / 个人资源包 / 组织资源包），
        //     桶级口径、没有时刻。
        //   `edition` —— 两家各是单一地区，但公开形态仍带 `edition` 供徽章显示。
        //   `email_as_name` —— 本家账号以邮箱报名字。
        .with_account_features(proto::AccountFeatures {
            usage: true,
            credit_detail: true,
            edition: true,
            identifier: vec!["uid".to_string(), "userId".to_string()],
            expiry: vec!["expiresAt".to_string()],
            email_as_name: true,
            ..Default::default()
        })
        .with_checkin(proto::CheckinInfo {
            // 两个地区链路不同：中国版是活动领取（每天 10:00 刷新），国际版
            // 要带设备风控身份领「每日 100 Credits」（UMID 组件）。
            description: Some(match self.region {
                Region::Cn => "活动（campaign）领取 · 每天 10:00 刷新".to_string(),
                Region::Global => {
                    "带设备风控身份领取「每日 100 Credits」 · 需要 UMID 组件".to_string()
                }
            }),
            modes: vec![proto::CheckinMode::Checkin],
            ui_hooks: match self.region {
                Region::Cn => Vec::new(),
                Region::Global => vec![proto::CheckinUiHook::UmidComponent],
            },
            ..Default::default()
        })
    }

    fn models<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async move {
            Ok(proto::ModelList {
                // 拆家后每家只列**本地区**的清单（不再并集）
                models: models::list_for(self.region),
                meta: proto::CatalogMeta {
                    remote: models::remote_refreshed(self.region),
                    refreshed_at: models::last_refreshed_at(self.region),
                },
            })
        })
    }

    /// 会话式转发入口：构造（编码 + 签名）→ 发送 → 翻译。
    fn conversation<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ConversationRequest,
        sink: ConversationSink<'a>,
    ) -> SendFuture<'a, ()> {
        Box::pin(run_conversation(ctx, self.region, req, sink))
    }

    /// 非 2xx 的上游响应分类。
    ///
    /// **注意**：Qoder 的业务错误不体现在 HTTP 状态码上（永远是 200，错误在
    /// SSE 信封里），所以这一层只处理真正的传输/网关层错误；流内的业务错误
    /// 由 `conversation` 直接转成带状态码的网关错误交回编排层。
    fn classify(&self, status: u16, error_body: &Value) -> proto::ErrorClass {
        let raw = error_body
            .get("message")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误");
        let classified = errors::classify_upstream_error(status, raw);
        let message = format!("上游返回 {status}: {raw}");
        match classified.kind {
            errors::UpstreamKind::Auth => proto::ErrorClass::TokenExpired { message },
            errors::UpstreamKind::Quota | errors::UpstreamKind::Rate => {
                proto::ErrorClass::QuotaLimited {
                    reset_at: None,
                    message,
                    upstream_code: error_body.get("code").and_then(Value::as_i64),
                    status: if status == 0 { 429 } else { status },
                }
            }
            // 内容策略拦截（审核文案）→ ContentBlocked：不罚账号，交给编排层换
            // 中性提示词重试一次 + 触发降级
            _ => content_block::classify_or_fatal(
                status,
                error_body,
                message,
                error_body.get("code").and_then(Value::as_i64),
            ),
        }
    }

    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            refresh::ensure_fresh(ctx, self.region, account, false)
                .await
                .map(|credentials| credentials.access_token)
                .map_err(GatewayError::into_message)
        })
    }

    /// 401 之后的**强制**续期：不看临期窗口。
    ///
    /// 与另外几家同一理由：401 完全可能发生在一个时间上还很新的 token 上
    /// （服务端侧失效 / 账号被顶下线 / refreshToken 轮换），此时只调 ensure
    /// 会拿回同一个被拒的 token。
    fn refresh_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            refresh::ensure_fresh(ctx, self.region, account, true)
                .await
                .map(|credentials| credentials.access_token)
                .map_err(GatewayError::into_message)
        })
    }

    /// 临期判定：取记录 → 凭证的 `expiring()`
    fn token_expiring<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, bool> {
        Box::pin(async move {
            if account.is_empty() {
                return Ok(false);
            }
            let record = match crate::store::record(ctx, self.region.provider_id(), account).await {
                Ok(record) if !record.is_null() => record,
                _ => return Ok(false),
            };
            Ok(credentials::Credentials::from_payload(&record)
                .map(|credentials| credentials.expiring())
                .unwrap_or(false))
        })
    }

    fn quota_sources(&self, report: &Value) -> Vec<Value> {
        crate::quota::sources(&self.meta().id, report)
    }

    fn usage<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, Value> {
        Box::pin(async move {
            balance::query(ctx, self.region, account)
                .await
                .map_err(GatewayError::into_message)
        })
    }

    /// 刷模型目录：用**本地区队首可用账号**的凭证（目录接口要签名，没有账号
    /// 就拿不到 —— 与源实现「必须登录」的前置条件一致）。
    ///
    /// `force` 一路透传：`false` 走 1 小时 TTL 早退（自动路径），`true` 真打上游。
    ///
    /// ── 没有账号时为什么返回「没刷」而不是「失败」──────────────────
    /// 手动刷新的逐家结果会显示在模型页上。一个从不用 Qoder 的用户点刷新时，
    /// 报一条红色失败会让他以为哪里坏了 —— 而事实只是「这家没有可刷的东西」。
    /// 三档语义里这正是 `unchanged`（skipped）的定义：**不是错误**。
    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            // 空 id = 本地区队首可用账号；非空 = 用户在弹窗里点名的那条
            let record = crate::store::record(ctx, self.region.provider_id(), account).await?;
            if record.is_null() {
                // 自动路径每次拉目录/启动都会走到这里，所以只打 verbose
                logging::verbose(
                    "[Models]",
                    &format!(
                        "Qoder {}模型目录刷新跳过：尚未添加账号",
                        self.region.label()
                    ),
                );
                if account.is_empty() {
                    return Ok(proto::RefreshOutcome::unchanged());
                }
                return Ok(proto::RefreshOutcome::failed(
                    "指定的账号不存在或不可用，请重新选择",
                ));
            }
            let Ok(credentials) = credentials::Credentials::from_payload(&record) else {
                return Ok(proto::RefreshOutcome::failed(
                    "Qoder 账号凭证无效，请重新登录或更新 PAT",
                ));
            };
            let proxy = match auth::account_proxy(&record) {
                Ok(proxy) => proxy,
                Err(error) => return Ok(proto::RefreshOutcome::failed(error.message)),
            };
            Ok(models::refresh(&credentials, proxy.as_ref(), force).await)
        })
    }

    /// 思考等级绑定 → `reasoning_effort`（值**原样**交给本家的归一逻辑）。
    ///
    /// ── 为什么本家不做档位映射（与 CatPaw 的差别，别照抄那边）──────
    /// 「这一家收哪些档位」在 Qoder 是**模型级**知识：上游目录每个模型条目自己
    /// 声明 `thinking_config.enabled.efforts`（Qwen3.8 系列是 low / medium /
    /// xhigh），`protocol::resolve_thinking` 已经按它归一与回退。在这里再折一层
    /// 等于把同一件事做两遍，而且**必然做错**：适配器这一层只看得到 `_model`
    /// 这个名字，拿不到那个模型声明的档位表。
    ///
    /// 所以这里只做两件事：判「客户端是不是已经指定了」、判「这个值值不值得
    /// 交给上游的归一逻辑」（判据是通用表的正向 6 档）：
    ///   - 表内值 → 注入。归并与回退交给 `resolve_thinking`；
    ///   - 表外的自定义值 → 不注入。本家对未知档位**不报错**（回退到模型默认
    ///     档），所以注入它不会弄坏请求 —— 但也**不会生效**，日志里却会出现一行
    ///     「已注入」。那正是「绑了没生效还查不出」的典型来源。
    fn reasoning_patch(&self, level: &str, _model: &str, body: &Value) -> proto::ReasoningPatch {
        if protocol::client_specified_reasoning(body) {
            return proto::ReasoningPatch::Skip {
                reason: "客户端请求体里已指定思考档位，绑定不覆盖".to_string(),
            };
        }
        if crate::model_rules::reasoning_rank(level).is_none() {
            return proto::ReasoningPatch::Skip {
                reason: "该等级不在通用候选表内，本家无法判断上游收不收（档位由各模型自己声明）"
                    .to_string(),
            };
        }
        proto::ReasoningPatch::Set {
            field: REASONING_FIELD.to_string(),
            value: Value::String(level.trim().to_string()),
        }
    }

    /// 从发送体读随行的思考等级（**插件里没有挂点**，如实说明）。
    ///
    /// ── 为什么这里没有实现 ────────────────────────────────────
    /// 网关的 `ProviderAdapter::outbound_reasoning` 是**同步**方法：编排层在
    /// 拼请求日志时要立刻拿到「这次发了什么档位」。插件协议里没有对应的
    /// 上报槽位（`Provider` trait 没有这个方法），网关的请求日志因此对插件家
    /// 显示不出「上游档位」这一列 —— 这是**已知的能力差**。
    ///
    /// 判据本身（`protocol::declared_reasoning` 的取值链）仍留在 `protocol.rs`：
    /// `resolve_thinking` 用它做「客户端已指定」的判定，那条路是转发热路径，
    /// 必须保留。

    // ── 签到 ──────────────────────────────────────────────────

    fn checkin<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(async move {
            let value = checkin::claim_daily_checkin(ctx, self.region, account)
                .await
                .map_err(GatewayError::into_control_message)?;
            Ok(checkin_report(&value))
        })
    }

    fn checkin_with_options<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        _manual: bool,
    ) -> SendFuture<'a, agent2api_provider_sdk::CheckinResult> {
        Box::pin(async move {
            let value = checkin::claim_daily_checkin(ctx, self.region, account).await
                .map_err(GatewayError::into_control_message)?;
            Ok(agent2api_provider_sdk::CheckinResult {
                report: checkin_report(&value),
                retry_at: value.get("retryAt").and_then(Value::as_i64),
            })
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    fn normalize_account<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(accounts::normalize(self.region, input))
    }

    fn account_view(&self, secret: &Value) -> Result<Value, String> {
        Ok(accounts::account_view(self.region, secret))
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(accounts::identity_of(secret))
    }

    // ── 登录（国际版 PKCE 设备授权 / 中国版 PAT）────────────────

    fn login_start<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        method: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(oauth::login_start(self.region, method, payload))
    }

    fn login_poll<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(oauth::login_poll(self.region, flow))
    }

    // ── 设备风控组件（UMID）──────────────────────────────────

    /// 组件状态与安装。
    ///
    /// ── 为什么走 `admin.action` 而不是独立方法 ───────────────────
    /// 组件是**机器级**的（一次安装服务本地区全部账号），既不是某个账号的
    /// 属性，也不属于任何一次转发 —— 塞进 `Provider` 的账号面或数据面都是
    /// 错位。`admin.action` 正是为这类「厂商专属、网关不该认识」的动作准备的
    /// 通用通道（与 ZCode 的 `claim.*`、CodeArts 的 `welfare.*` 同款）。
    ///
    /// ── 返回的是**网关的**形状 ────────────────────────────────
    /// 前端读 `api::qoder_umid` 的既有键名（`available` / `source` /
    /// `installSupported` / `installing`），形状一变界面就白屏，所以逐字复刻。
    fn admin_action<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
        action: &'a str,
        _payload: &'a Value,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            match action {
                "risk.status" => Ok(risk::status(self.region)),
                "risk.install" => risk::install_component()
                    .await
                    .map_err(GatewayError::into_message),
                other => Err(format!(
                    "Qoder 不认识管理动作「{other}」（可用：risk.status / risk.install）"
                )),
            }
        })
    }
}

/// 会话式转发的实现（从网关的 `forward_conversation` 搬来，改成 sink 出口）。
///
/// ── 错误处理与编排层的分工 ──────────────────────────────────
/// 上游的业务错误藏在 SSE 信封里（HTTP 200），所以「换账号重试」这个动作
/// 必须由本函数在读到信封错误时**主动转成网关错误**交回编排层：额度/限流
/// 报 429、鉴权报 401，编排层据此走它既有的三档动作（换账号 / 刷新重试）。
/// 流已经开始下发之后的错误只能留在流里（HTTP 头早发出去了）——
/// 那时补一帧 `{"error":…}` + `[DONE]`，与通用层的断流收尾同一形态。
async fn run_conversation(
    ctx: &PluginCtx,
    region: Region,
    req: proto::ConversationRequest,
    mut sink: ConversationSink<'_>,
) -> Result<(), String> {
    run_conversation_inner(ctx, region, req, &mut sink)
        .await
        .map_err(|error| error.message)
}

async fn run_conversation_inner(
    ctx: &PluginCtx,
    region: Region,
    req: proto::ConversationRequest,
    sink: &mut ConversationSink<'_>,
) -> Result<(), GatewayError> {
    let account_id = req.account.clone();
    let body = req.body;
    let stream = req.stream;
    let model_name = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // 插件里的遥测是空实现（见 `telemetry` 模块头）：调用点逐字保留，行为差异
    // 集中在那一个文件里说明。
    let telemetry = std::sync::Arc::new(RequestTelemetry::new());

    // 凭证（含临期主动刷新）。显式账号必填 —— Qoder 没有环境变量旁路。
    if account_id.is_empty() {
        return Err(GatewayError::with_status(
            503,
            "没有可用的 Qoder 账号：请在账号页添加并启用账号",
        ));
    }
    let mut context = chat::account_context(ctx, region, &account_id, false).await?;
    // 代理：请求里给的优先（网关已解析好），没有就用账号记录里的
    let effective_proxy = req.proxy.clone().or(context.proxy.clone());
    // 排队退避的预算：用户给「等待响应超时」的那份预算（理由见 queue_backoff）
    let queue_budget_ms = crate::timeouts::current().headers_ms();
    let mut queue_waits: usize = 0;
    let mut queue_waited_ms: u64 = 0;
    // 鉴权失败后的强制续期只做一次（见 refresh_after_auth）
    let mut auth_retries: usize = 0;

    // ── 为什么这里是一个循环 ────────────────────────────────
    // 上游对免费模型走排队制，繁忙时会用 403 + 业务码 10605 回一句
    // 「暂不可服务，建议 N 秒后再来」。那是**可以原地等一会儿重发**的状态，
    // 所以「构造 → 发送 → 读首帧」这一整段每轮重跑一次：排队就睡一觉再来，
    // 其它错误按原样返回。首帧之后（流已开始下发）不再重试。
    loop {
        // 每轮都重新构造：COSY 签名覆盖 request_id 与请求体，**重放同一份签名**
        // 会被上游判成重复请求（403 / code 103），所以退避之后必须换一份新的
        // request_id 与签名。
        let plan = chat::build_plan(&context.credentials, &body, &model_name)?;

        logging::verbose(
            "[Qoder]",
            &format!(
                "POST {} model={} upstream={} stream={} region={} account={}",
                plan.url,
                if model_name.is_empty() {
                    "(默认)"
                } else {
                    &model_name
                },
                plan.upstream_key,
                stream,
                context.credentials.region.id(),
                account_id,
            ),
        );

        let response = chat::send(&plan, effective_proxy.as_ref()).await?;

        if !response.status().is_success() {
            // 只有失败响应才读体（成功的是 SSE 流，读了就没流了）
            let status = response.status().as_u16();
            match chat::http_error(status, response).await {
                // 鉴权类：强制续期凭证后重发（一次）
                chat::AttemptError::Auth(error) => {
                    context = refresh_after_auth(
                        ctx,
                        region,
                        &account_id,
                        &mut auth_retries,
                        error,
                        &telemetry,
                    )
                    .await?;
                    continue;
                }
                chat::AttemptError::Fatal(error) => {
                    // 限额信号落冷却（见 `record_limited`）
                    if error.status_code == 429 {
                        record_limited(
                            ctx,
                            &account_id,
                            &model_name,
                            status,
                            errors::UpstreamKind::Quota,
                            &error.message,
                        )
                        .await;
                    }
                    return Err(error);
                }
                chat::AttemptError::Queued(queued) => {
                    queue_backoff(
                        &mut queue_waits,
                        &mut queue_waited_ms,
                        queue_budget_ms,
                        &queued,
                        &telemetry,
                    )
                    .await?;
                    continue;
                }
            }
        }

        let translator = Translator::new(response_id(), plan.model_name.clone(), plan.thinking);
        // 限额记账的素材：流式分支的收尾发生在 handler 返回之后，那时这里的
        // 局部变量都还在（被 move 进后台任务），所以先克隆好
        let limit_ctx = LimitContext {
            ctx,
            account_id: account_id.clone(),
            model: model_name.clone(),
        };
        if stream {
            // ── 首帧预读（issue #8 的修复，见 piping 模块头）────────
            // 拿到 HTTP 200 不能直接报头：Qoder 的额度错误恰恰写在 200 的 SSE
            // 信封里。预读把首个事件拦在报头之前 —— 业务错误转成带状态码的
            // Err 交回编排层换号（此刻还没有任何字节下发，换号无损）；拿到
            // 内容帧才报头，预读帧随后补发。
            match piping::prefetch_stream_head(response, &limit_ctx, &telemetry).await {
                Ok((prefetched, source)) => {
                    sink.head(proto::StreamHead {
                        status: 200,
                        headers: [(
                            "content-type".to_string(),
                            "text/event-stream".to_string(),
                        )]
                        .into_iter()
                        .collect(),
                        upstream: proto::UpstreamKind::Chat,
                    })
                    .await
                    .map_err(|error| GatewayError::with_status(499, error))?;
                    // 上游流必须被**拉到底**：客户端断开时 `sink.send` 会失败，
                    // 循环随即退出，drop 掉 source 就等价于断开上游连接。
                    return piping::drive_stream(
                        source,
                        translator,
                        telemetry,
                        limit_ctx,
                        sink,
                        prefetched,
                    )
                    .await
                    .map_err(|error| GatewayError::with_status(502, error));
                }
                // 首帧就是排队信封（HTTP 200 + statusCodeValue 403）：还没报头，
                // 退避重发（和 HTTP 层的排队走同一条路）
                Err(chat::AttemptError::Queued(queued)) => {
                    queue_backoff(
                        &mut queue_waits,
                        &mut queue_waited_ms,
                        queue_budget_ms,
                        &queued,
                        &telemetry,
                    )
                    .await?;
                    continue;
                }
                Err(chat::AttemptError::Auth(error)) => {
                    context = refresh_after_auth(
                        ctx,
                        region,
                        &account_id,
                        &mut auth_retries,
                        error,
                        &telemetry,
                    )
                    .await?;
                    continue;
                }
                Err(chat::AttemptError::Fatal(error)) => return Err(error),
            }
        }

        // 非流式：内部仍走流式拉取（上游只支持流式），再聚合成完整响应
        match drive_aggregate(response, translator, telemetry.clone(), &limit_ctx).await {
            Ok(body) => {
                let payload = serde_json::to_string(&body).map_err(|error| {
                    GatewayError::with_status(500, format!("Qoder 聚合结果序列化失败：{error}"))
                })?;
                sink.head(proto::StreamHead {
                    status: 200,
                    headers: [(
                        "content-type".to_string(),
                        "application/json".to_string(),
                    )]
                    .into_iter()
                    .collect(),
                    upstream: proto::UpstreamKind::Chat,
                })
                .await
                .map_err(|error| GatewayError::with_status(499, error))?;
                sink.send(payload.as_bytes())
                    .await
                    .map_err(|error| GatewayError::with_status(499, error))?;
                return Ok(());
            }
            // 非流式这一路也还没报头：排队同样可以退避重发
            Err(chat::AttemptError::Queued(queued)) => {
                queue_backoff(
                    &mut queue_waits,
                    &mut queue_waited_ms,
                    queue_budget_ms,
                    &queued,
                    &telemetry,
                )
                .await?;
                continue;
            }
            Err(chat::AttemptError::Auth(error)) => {
                context = refresh_after_auth(
                    ctx,
                    region,
                    &account_id,
                    &mut auth_retries,
                    error,
                    &telemetry,
                )
                .await?;
                continue;
            }
            Err(chat::AttemptError::Fatal(error)) => return Err(error),
        }
    }
}

fn checkin_report(value: &Value) -> proto::CheckinReport {
    let outcome = if value.get("alreadyCompleted").and_then(Value::as_bool) == Some(true) {
        proto::CheckinOutcome::Done
    } else if value.get("success").and_then(Value::as_bool) == Some(true) {
        proto::CheckinOutcome::Claimed
    } else if value.get("outcome").and_then(Value::as_str) == Some("inactive") {
        proto::CheckinOutcome::Inactive
    } else {
        proto::CheckinOutcome::Failed
    };
    proto::CheckinReport {
        outcome,
        credit: value.get("rewardPoints").and_then(Value::as_f64)
            .filter(|points| *points > 0.0).map(|points| format!("{points} credits")),
        streak: None,
        message: value.get("msg").and_then(Value::as_str).map(str::to_string),
    }
}

/// 鉴权类失败后的补救：**强制**续期凭证一次，成功则交回新快照供调用方重发。
///
/// ── 为什么是「强制」而不是 `ensure` ──────────────────────────
/// `ensure_fresh(force = false)` 只在临期窗口内才真续期。而 401 完全可能发生在
/// 一个**时间上还很新**的 token 上（服务端侧作废、被别处顶下线、refreshToken
/// 轮换过）—— 那时 `ensure` 会把同一个被拒的 token 原样交回来，白试一次。
/// 这与无状态路径的 `TokenExpired` 动作同一理由（见 `providers::adapter` 模块头）。
///
/// ── 为什么只做一次 ──────────────────────────────────────────
/// 续出来还被拒，说明问题不在凭证新旧（权限、地区、模型未开通）。再刷就是
/// 对着同一个结果反复重试，还会把上游的刷新接口打成限流的形状。
///
/// 续期失败时返回的是**续期自己的错误**（「刷新令牌已失效，请重新登录」这类），
/// 它比「上游返回 401」更能告诉用户下一步该做什么。
async fn refresh_after_auth(
    ctx: &PluginCtx,
    region: Region,
    account_id: &str,
    attempts: &mut usize,
    error: GatewayError,
    telemetry: &std::sync::Arc<RequestTelemetry>,
) -> Result<chat::AccountContext, GatewayError> {
    if *attempts > 0 {
        return Err(error);
    }
    *attempts += 1;
    logging::log("[Qoder]", "🔑 上游报鉴权失败，强制续期凭证后同账号重试一次");
    // 续期本身可能在途（刷新接口一次往返），先记一条内部重试 —— 用户看到
    // 「凭证过期 → 续期 → 重发」这条链，才解释得通这次请求为什么慢了一拍
    telemetry.note_attempt_retry("上游鉴权失败，强制续期凭证后重试", Some(401), 0);
    chat::account_context(ctx, region, account_id, true).await
}

// ── 排队等待的次数与单次时长来自设置页「通用 → 排队等待」──────────
// （`queueMaxWaits` / `queueWaitSeconds`，见 `config::queue_settings`）：
// 缺省最多等 2 次、单次跟随上游建议（9～30 秒，上游没给建议时 15 秒）；
// 把次数写成 0 = 不等待，排队态立刻以 503 返回（「快速失败、客户端自己重试」
// 那一档，参考实现里叫 `queue_max_waits: 0`）。

/// 排队退避：还能等就睡一觉并返回 `Ok`（调用方重新签名再发一次）；
/// 预算用尽返回 `Err`（排队落定成 503，文案带已等待时长与上游原文）。
///
/// ── 等待预算为什么要看「等待响应超时」────────────────────────
/// 排队等待发生在**首帧之前**（客户端的 200 头还没发出），吃掉的是用户在设置页
/// 给「等待响应超时」的那份预算。用户把它调小（例如 20 秒）就说明他不接受一次
/// 请求卡那么久 —— 那就不等，直接给一条能看懂的排队错误。
async fn queue_backoff(
    waits: &mut usize,
    waited_ms: &mut u64,
    budget_ms: u64,
    queued: &chat::Queued,
    telemetry: &std::sync::Arc<RequestTelemetry>,
) -> Result<(), GatewayError> {
    let settings = crate::queue::queue_settings();
    let max_waits = settings.wait_budget();
    // 单次时长：设置里写死了就用它，否则跟随上游建议（`queued.retry_after_ms`
    // 已经在分类时按 5～30 秒钳过）
    let wait_ms = settings.forced_wait_ms().unwrap_or(queued.retry_after_ms);
    if *waits >= max_waits || waited_ms.saturating_add(wait_ms) > budget_ms {
        return Err(chat::queued_error(queued, *waited_ms));
    }
    *waits += 1;
    *waited_ms += wait_ms;
    let seconds = wait_ms / 1000;
    logging::log(
        "[Qoder]",
        &format!(
            "⏳ 上游模型排队中{}，{seconds} 秒后重试（第 {}/{} 次）",
            if queued.detail.is_empty() { String::new() } else { format!("（{}）", queued.detail) },
            *waits,
            max_waits,
        ),
    );
    // 请求日志的尝试明细 + 阶段：排队退避既是一次「内部重试」，也是一段独立的
    // 等待（阶段显示成「排队中」）—— 用户能在详情里看到「这次为什么慢」，
    // 否则这段等待会被读成「网关卡住了」。
    telemetry.note_queued("上游模型排队中", Some(10605), wait_ms, *waits, max_waits);
    sleep_or_cancel(telemetry, wait_ms).await;
    Ok(())
}

/// 可取消的等待：客户端把请求掐掉（或用户点了终止）时立刻结束。
///
/// 与无状态路径的退避同一形态（见 `upstream::provider_loop` 的 `sleep_or_cancel`）：
/// 令牌未接线时退化成普通 sleep，行为与「不可取消」一致。
async fn sleep_or_cancel(_telemetry: &std::sync::Arc<RequestTelemetry>, delay_ms: u64) {
    // ── 迁移时改了什么 ────────────────────────────────────────
    // 网关那份在这里 `select!` 客户端的取消令牌（用户点终止就立刻结束等待）。
    // 插件**拿不到那个令牌**（见 `telemetry` 模块头第 3 条），所以退化成
    // 普通 sleep：客户端断开后，这次退避仍会睡完才发现发不出去（最多一次
    // 退避时长，通常 9～30 秒）。这是已知的行为差。
    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
}

/// 流式链路的限额记账素材：预读的首帧错误（`piping::prefetch_stream_head`，
/// handler 返回前）与 `piping::drive_stream` 的中途错误都在错误现场落冷却；
/// 后者在 handler 返回之后才跑，那些标识必须随任务一起带走。
struct LimitContext<'a> {
    /// 借用当前调用的 ctx：插件里 `conversation` 是**推模型**（`drive_stream`
    /// 被直接 await，不是后台任务），所以不需要 `'static`。网关那份要 `String`
    /// 是因为它把 `drive_stream` 挂进 `spawn_task` —— 插件没有那个形态。
    ctx: &'a PluginCtx,
    account_id: String,
    model: String,
}

/// 非流式：拉完整条上游流，聚合成一个完整 `chat.completion`。
///
/// 与流式路径的关键差别：这里的错误**还没有下发任何内容**（HTTP 头都没发），
/// 所以额度类错误可以带着状态码交回编排层、排队态可以退避重发，都由调用方
/// 决定（见 `forward_conversation` 的循环）。冷却标记在调用方落（它持有 store
/// 与账号标识的完整上下文）。
async fn drive_aggregate(
    response: reqwest::Response,
    mut translator: Translator,
    telemetry: std::sync::Arc<RequestTelemetry>,
    limit: &LimitContext<'_>,
) -> Result<Value, chat::AttemptError> {
    use futures::StreamExt;

    let mut lines = stream::LineBuffer::new();
    let mut source = crate::response_stream::sse(response);
    let mut business_failure: Option<chat::AttemptError> = None;
    let mut transport_failure: Option<chat::AttemptError> = None;
    let mut completed = false;
    // 调试模式的采集器（与 drive_stream 同一位置：解析之前采原始字节）
    let capture = telemetry.capture();

    'outer: while let Some(item) = source.next().await {
        let chunk = match item {
            Ok(chunk) => chunk,
            Err(error) => {
                transport_failure = Some(chat::AttemptError::Fatal(GatewayError::with_status(
                    502, format!("Qoder 上游流式传输中断: {error}"),
                )));
                break;
            }
        };
        if let Some(capture) = capture.as_deref() {
            capture.push(&chunk);
        }
        for data in lines.push(&chunk) {
            match stream::parse_sse_line(&data) {
                SseEvent::Skip => {}
                SseEvent::Done => { completed = true; break 'outer; }
                SseEvent::Error { status, kind, raw, message, pricing_url, queue } => {
                    record_limited(
                        &limit.ctx,
                        &limit.account_id,
                        &limit.model,
                        status,
                        kind,
                        &message,
                    )
                    .await;
                    business_failure = Some(chat::business_error(
                        status,
                        kind,
                        &raw,
                        &message,
                        pricing_url.as_deref(),
                        queue,
                    ));
                    break 'outer;
                }
                SseEvent::Chunk(chunk) => {
                    translator.consume(&chunk, Some(&telemetry));
                }
            }
        }
    }
    if business_failure.is_none() && !completed {
        for data in lines.finish() {
            if let SseEvent::Chunk(chunk) = stream::parse_sse_line(&data) {
                translator.consume(&chunk, Some(&telemetry));
            }
        }
    }
    // 聚合路径下这一档错误是**终态**：它能带着状态码交回编排层，由编排层决定
    // 换账号还是刷新重试（这是与流式路径的关键差别 —— 流式时 HTTP 头已发出）
    if let Some(error) = business_failure.or(transport_failure) {
        return Err(error);
    }
    // 冲刷拆解器的尾巴，再成形（少了这一步，回答末尾会少几个字符）
    translator.finish();
    Ok(translator.completion_body())
}
