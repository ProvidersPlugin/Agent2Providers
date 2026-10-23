//! Command Code（`api.commandcode.ai`）插件实现：**无状态转发 + 粘贴式 API Key**。
//!
//! ── 从网关搬来时改了什么（逐条对照，便于 parity 核对）────────────
//! ```text
//!   不变：8 键信封、params 逐字段改写、会话派生、指纹派生、CLI 客户端头、
//!         错误分类映射、26 项内置兜底清单、5 分钟 TTL、预请求节流
//!   改：  ① 凭证从 `store.commandcode_account_record()` 改成 `ctx.store_get()`
//!            —— 插件架构下账号库在网关侧，插件只能走回调；
//!         ② 目录缓存**不落盘**（网关自己会缓存插件返回的清单）；
//!         ③ `refresh_models` 的「没有账号」从 `unchanged()` 改成**用内置清单
//!            刷新**（插件不知道网关有没有账号，而它的兜底清单本来就是可用的）；
//!         ④ 账号添加（`normalize_account`）从网关的 `api/accounts.rs` 分支
//!            搬进来，含粘贴归一化与 best-effort 探活。
//! ```
//!
//! ── 上游的三处硬事实 ────────────────────────────────────────
//! ```text
//!   1. 生成走 POST /alpha/generate，请求体是**自有信封**
//!      （config / memory / taste / skills / permissionMode / [threadId] / mode / params），
//!      而不是 OpenAI 的 {model, messages}；
//!   2. 响应是 **NDJSON**（一行一个 JSON 事件），且 **HTTP 恒 200** ——
//!      认证失败 / 限流 / 余额不足都在流内 {"type":"error"} 表达；
//!   3. 发正式请求前必须先上报「设备指纹 + 生命周期」两条预请求。
//! ```
//!
//! ── 响应为什么走 `UpstreamKind::Ndjson` 而不是会话式转发 ─────────
//! 第 2 条只需在「上游响应协议」上选一个变体，由网关的翻译层把 NDJSON 折成
//! 标准 chat SSE —— 账号轮换、限额冷却、退避重试、usage 与取消处理全部留在
//! 网关编排层。若改走 `conversation`，那五样都要在插件里重写一遍，且本家
//! 并没有多步会话协议，没有理由付那份代价。**本家 `stateful` 恒 false。**
//!
//! ── 本家没有的东西（如实声明，别照抄别家）──────────────────────
//!   - **没有续期**：单个静态 API Key，上游没有 refresh 接口 → `refresh = false`；
//!   - **没有网页登录**：粘贴式凭证，不开窗口（`web_login` 默认 false）；
//!   - **不做签到**：没有可自动领的奖励活动（`checkin = false`）；
//!   - **不做余额查询**：`/alpha/billing/*` 只在添加账号时用来探活
//!     （`usage = false`）。

pub mod credentials;
pub mod endpoints;
pub mod fingerprint;
pub mod models;
pub mod plan;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ChatPlan, PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::{json, Value};

/// provider id（`pack.json` 里也要写同一个）
pub const ID: &str = "commandcode";

/// Command Code 适配器
pub struct CommandCodeProvider;

impl Provider for CommandCodeProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(ID, "Command Code")
            .with_capabilities(proto::Capabilities {
                chat: true,
                // 无状态：一次 HTTP 请求 = 一次生成（响应虽是 NDJSON，但那属于
                // 「上游响应协议」，不是会话状态机）
                stateful: false,
                // 静态 key，上游没有 refresh 接口
                refresh: false,
                model_refresh: true,
                // 不查余额：billing 端点只在添加账号时探活用
                usage: false,
                // 没有可自动领的奖励活动
                checkin: false,
                claims: false,
                // 粘贴式凭证，不开窗口
                web_login: false,
                // NDJSON 的翻译层自己处理 model 字段，不需要 SSE 改写
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
                label: "API Key".into(),
                placeholder: "粘贴 user_ 开头的 API Key".into(),
                extra_fields: vec![],
            }])
            // 内置兜底清单（没有远程目录时用它）
            .with_models(models::fallback_models())
            // 账号行：`usage: false` —— 本家**没有余额查询**：`/alpha/billing/*`
            // 只在添加账号时用来探活（见 `adapter` 的模块头），所以如实关掉
            // （与 `Capabilities::usage` 同一口径）。
            //   `edition: false` —— 单一入口。
            //   `identifier: id` —— 没有可读的用户标识：账号 id 由 key 的 SHA-256
            //     前 12 位拼成（`commandcode-<hash>`），所以标识列显示这个 id
            //     本身，与 officeace 同一处置。
            //   `expiry` 空 + `long_lived: true` —— 单个静态 API Key，上游没有
            //     refresh 接口、key 本身也没有到期时刻。
            .with_account_features(proto::AccountFeatures {
                identifier: vec!["id".to_string()],
                long_lived: true,
                ..Default::default()
            })
    }

    fn provider_id(&self) -> String {
        ID.to_string()
    }

    // ── 无状态转发 ────────────────────────────────────────────

    fn build_chat<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ChatRequest,
    ) -> SendFuture<'a, ChatPlan> {
        Box::pin(async move {
            // ── 取凭证 ──────────────────────────────────────────
            // 网关在调用 `chat` 之前已经调过 `token.ensure`（它拿到的 token
            // 会放进 `req.body` 的鉴权头吗？不会 —— 网关只负责发请求，鉴权头
            // 在这里拼）。所以这里必须自己读一次账号。
            let account = ctx
                .store_get(ID, &req.account)
                .await
                .map_err(|error| format!("读取 Command Code 账号失败：{error}"))?;
            let api_key = credentials::api_key_of(&account.secret)?;

            // ── 构造 8 键信封 ───────────────────────────────────
            let built = plan::build(&api_key, &req.body, &req.headers);
            Ok(ChatPlan {
                url: built.url,
                headers: built.headers,
                body: built.body,
                // NDJSON：网关据此选 `CommandCodeToChatStream` 翻译层
                upstream: proto::UpstreamKind::Ndjson,
            })
        })
    }

    /// 上游错误分类（规格 §7.1 的映射表）：
    ///   - 400 / 422 → `Fatal(400)`（请求体非法，不重试；命中内容拦截文案时
    ///     走共用的内容拦截判定 —— 那档不罚账号、换提示词补救）；
    ///   - 401 / 403 → `TokenExpired`（编排层刷新一次（本家刷新必然拿回同一个
    ///     key）再换账号）；
    ///   - 402 → `QuotaLimited`（**下游状态码按 429**：规格明确「付费失败按限流」）；
    ///   - 404 → `Fatal(404)`；429 → `QuotaLimited(429)`；
    ///   - 500 / 502 → `Fatal(502)`；503 → `Fatal(503)`；其余 → `Fatal(502)`。
    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        let raw = body
            .get("message")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误");
        let message = format!("上游返回 {status}: {raw}");
        let code = body.get("code").and_then(Value::as_i64);
        match status {
            400 | 422 => content_or_fatal(400, body, message, code),
            401 | 403 => proto::ErrorClass::TokenExpired { message },
            402 => proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message: format!(
                    "{message}（余额 / 套餐不足：请在 commandcode.ai 充值或换一个账号，本网关按限流处理）"
                ),
                upstream_code: code,
                // 规格的有意映射：402 → 下游 429
                status: 429,
            },
            429 => proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: code,
                status: 429,
            },
            404 => proto::ErrorClass::Fatal {
                status: 404,
                message,
                upstream_code: code,
            },
            503 => proto::ErrorClass::Fatal {
                status: 503,
                message,
                upstream_code: code,
            },
            // 500 / 502 与未列出的一切 → 502（规格的兜底）
            _ => proto::ErrorClass::Fatal {
                status: 502,
                message,
                upstream_code: code,
            },
        }
    }

    // ── 凭证 ──────────────────────────────────────────────────

    /// 取可用 API Key，并在此之前确保**设备指纹已上报**。
    ///
    /// 没有续期：这里只做「记录里有没有 key、形态对不对」的检查，
    /// 上游拒了就是 401，网关把它归成 `TokenExpired`。
    fn ensure_token<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, String> {
        Box::pin(async move {
            let found = ctx
                .store_get(ID, account)
                .await
                .map_err(|error| format!("读取 Command Code 账号失败：{error}"))?;
            let api_key = credentials::api_key_of(&found.secret)?;
            // best-effort：失败只告警，绝不影响生成（见 `fingerprint.rs`）
            fingerprint::ensure_initialized(&api_key, &ctx.env.proxy).await;
            Ok(api_key)
        })
    }

    /// 没有续期手段（静态 key，上游无 refresh 接口）—— 保持默认转调
    /// `ensure_token`：拿回的是同一个 key，网关据此判「刷新无效 → 提示重贴」。

    // ── 目录 ──────────────────────────────────────────────────

    fn models<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async move { Ok(models::list()) })
    }

    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            // 空账号 = 用本家第一个可用账号（与网关版同口径）
            let api_key = match ctx.store_get(ID, account).await {
                Ok(found) => match credentials::api_key_of(&found.secret) {
                    Ok(key) => key,
                    // 账号存在但凭证坏了：如实报失败（不是「没有账号」）
                    Err(reason) => return Ok(proto::RefreshOutcome::failed(reason)),
                },
                Err(_) if account.is_empty() => {
                    // ── 与网关版的一处有意差别 ──────────────────────
                    // 网关版在这里返回 `unchanged()`（「用户还没添加账号」是正常
                    // 状态）。插件侧拿不到「网关有没有账号」这个事实，而它手里
                    // 有一份**可用的内置兜底清单** —— 直接用它刷新比静默跳过
                    // 更有价值：界面能看到 26 个可点选的模型。
                    agent2api_provider_sdk::log_info!(
                        "[Models] Command Code 没有可用账号，模型目录使用内置清单"
                    );
                    return Ok(proto::RefreshOutcome::refreshed(models::fallback_models().len()));
                }
                Err(error) => {
                    return Ok(proto::RefreshOutcome::failed(format!(
                        "读取 Command Code 账号失败：{error}"
                    )))
                }
            };
            Ok(models::refresh(&api_key, force, &ctx.env.proxy).await)
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    /// 规范化一条账号：粘贴归一化（剥引号 / `Authorization: Bearer …` 整行 /
    /// 尾部杂字段）+ 形态检查 + best-effort 探活。
    ///
    /// ── 校验顺序（先本地、再线上）是刻意的 ──────────────────────
    /// 本地形态不对时**一次网络都不发**（用户粘错了东西，探活没有意义）；
    /// 线上探活失败**不阻断添加**（见 `credentials::probe_key` 的三档语义）。
    fn normalize_account<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move {
            let (raw, name) = match &input.source {
                proto::NormalizeSource::Manual { payload } => {
                    let raw = payload
                        .get("apiKey")
                        .or_else(|| payload.get("accessToken"))
                        .or_else(|| payload.get("token"))
                        .or_else(|| payload.get("key"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = payload
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string);
                    (raw, name)
                }
                proto::NormalizeSource::FileText { content, .. } => {
                    // 文件可能是裸 key，也可能是 `{"apiKey": "user_..."}`
                    let trimmed = content.trim();
                    if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
                        let raw = parsed
                            .get("apiKey")
                            .or_else(|| parsed.get("accessToken"))
                            .or_else(|| parsed.get("token"))
                            .and_then(Value::as_str)
                            .unwrap_or(trimmed)
                            .to_string();
                        (raw, None)
                    } else {
                        (trimmed.to_string(), None)
                    }
                }
                proto::NormalizeSource::FileBase64 { .. } => {
                    return Err("Command Code 不支持二进制文件导入，请粘贴 API Key".to_string());
                }
                proto::NormalizeSource::Desktop => {
                    return Err(
                        "Command Code 不支持导入桌面端登录态，请粘贴 API Key（user_ 开头）添加账号"
                            .to_string(),
                    );
                }
            };
            if raw.trim().is_empty() {
                return Err(format!(
                    "请粘贴 Command Code 的 API Key（{} 开头）",
                    credentials::KEY_PREFIX
                ));
            }
            // 输入长度上限（超过这个长度的输入一定是粘错了东西）
            if raw.chars().count() > 8192 {
                return Err("粘贴的内容过长，请确认复制的是 API Key 本身".to_string());
            }
            let api_key = normalize_key(&raw);
            if api_key.is_empty() {
                return Err(format!(
                    "请粘贴 Command Code 的 API Key（{} 开头）",
                    credentials::KEY_PREFIX
                ));
            }
            if !credentials::shape_ok(&api_key) {
                return Err(format!(
                    "这不是 Command Code 的 API Key：应以 {} 开头、只含字母数字与 _ -；\
                     请在 commandcode.ai/studio（或本机 ~/.commandcode/auth.json）重新复制",
                    credentials::KEY_PREFIX
                ));
            }
            // best-effort 探活：只有明确 401/403 才判无效
            match credentials::probe_key(&api_key, &ctx.env.proxy).await {
                credentials::KeyCheck::Valid => {}
                credentials::KeyCheck::Invalid(reason) => {
                    return Err(format!(
                        "Command Code API Key 无效或已失效（{reason}）：\
                         请在 commandcode.ai/studio 重新获取后粘贴"
                    ));
                }
                credentials::KeyCheck::Unknown(note) => {
                    // 判不出来就放行：一次上游抖动不该让用户加不进账号
                    agent2api_provider_sdk::log_info!(
                        "[Login] Command Code API Key 未能线上确认（{note}），按可用处理"
                    );
                }
            }
            // 稳定身份 = key 的摘要前缀（上游没有 userId 可解析）
            let identity = format!("commandcode-{}", short_hash(&api_key));
            let display = input
                .name
                .clone()
                .or(name)
                .unwrap_or_else(|| format!("Command Code {}", credentials::tail_of(&api_key)));
            Ok(proto::NormalizedAccount {
                secret: json!({ "apiKey": api_key }),
                public: json!({
                    "name": display,
                    "tokenTail": credentials::tail_of(&api_key),
                }),
                identity,
                desktop: false,
            })
        })
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        let api_key = credentials::api_key_of(secret)?;
        Ok(format!("commandcode-{}", short_hash(&api_key)))
    }
}

/// 粘贴归一化：剥引号 / 整行前缀 / 尾部杂字段，返回**裸 key**。
///
/// 认得的形态：裸 key、带成对引号、`Authorization: Bearer user_…`、
/// `x-api-key: user_…`、整行 `Cookie: …`、以及 key 后面还跟着别的字段
/// （`…user_xxx; Path=/` 之类）。做法是**扫描 `user_` 并取其后连续合法字符**
/// （正则 `user_[a-zA-Z0-9_-]+` 的手写形态）—— 这一步同时完成前缀剥离与
/// 尾部截断。扫不到 `user_` 时只剥引号与空白，交给形态检查给出可读的报错。
fn normalize_key(raw: &str) -> String {
    let trimmed = raw
        .trim()
        .trim_matches(|ch| matches!(ch, '"' | '\'' | '`' | ',' | ';'))
        .trim();
    if let Some(key) = extract_key(trimmed) {
        return key;
    }
    // 没有 `user_` 形态：仍把空白剥掉（key 里不含空白）
    trimmed.chars().filter(|ch| !ch.is_whitespace()).collect()
}

/// 扫描第一个 `user_` 并取其后连续合法字符
fn extract_key(text: &str) -> Option<String> {
    let prefix = credentials::KEY_PREFIX;
    let start = text.find(prefix)?;
    let rest = &text[start + prefix.len()..];
    let body: String = rest
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '-')
        .collect();
    if body.is_empty() {
        return None;
    }
    Some(format!("{prefix}{body}"))
}

/// key 的短摘要（16 位 hex，用于账号 id；不可反推）
fn short_hash(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}

/// 内容拦截的文案特征（全部小写；匹配前把上游文案转小写）
///
/// ── 为什么这份规则在插件里重复了一份 ──────────────────────────
/// 网关的 `providers::content_block` 是**网关内部**模块，插件看不到。
/// 这段规则本身是「上游侧知识」，按分层约定本就属于 provider —— 搬过来
/// 是归位，不是复制。
const CONTENT_BLOCK_PATTERNS: &[&str] = &[
    "blocked by security policy",
    "unapproved channel",
    "illegal api invocation",
];

/// 内容拦截的提示文案（附在上游原文之后，给用户两条可执行的路）
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
