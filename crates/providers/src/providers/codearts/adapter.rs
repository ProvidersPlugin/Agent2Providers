//! CodeArts（华为云 AI 代码助手 / snap-access）的 `Provider` 实现。
//!
//! ── 本文件只做装配 ──────────────────────────────────────────
//! 请求构造（签名 + 会话头）、流内错误判定、首包门、聚合都在 `chat.rs`；会话
//! 心跳与本地准入在 `session.rs`；续期在 `refresh.rs`；账号面在 `accounts.rs`；
//! 登录在 `login.rs` + `oauth.rs` + `callback_server.rs`；福利与余额在
//! `welfare.rs` / `balance.rs` / `onboarding.rs`；目录在 `models.rs`。
//!
//! ── 上游要点（决定了这里为什么这么接）───────────────────────
//!   * 每个请求都用 **SDK-HMAC-SHA256** 签名（AK/SK + STS 临时凭据，见
//!     `signer.rs`）。签名覆盖到「出门那串字节」，所以模型名、会话 id、改写过的
//!     `tools` 都必须先定下来再签。
//!   * **有状态**：上游对每个账号只允许 3 路并发对话，靠
//!     `chat-session/heartbeat` 的 busy/idle 记账（见 `session.rs`）。
//!   * 临时凭据只活约一小时，到期前 15 分钟就要续，而 refresh_token 是
//!     **一次性**的 → 续期必须单飞 + 写回（见 `refresh.rs`）。
//!   * 业务失败常常不体现在状态码上（额度耗尽是 HTTP 200 + 流内信封），
//!     所以首包门与 `stream_fault` 是必需的一层。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap / expect / panic。

use std::sync::OnceLock;

use serde_json::Value;

use agent2api_provider_protocol as proto;
use agent2api_provider_protocol::WireProxy as ResolvedProxy;
use agent2api_provider_sdk::{ConversationSink, PluginCtx, Provider, ProviderMeta, SendFuture};

use crate::errors::GatewayError;
use crate::logging;
use crate::proxy_util::{resolve_account_proxy, ProxyResolution};
use crate::telemetry::RequestTelemetry;

use super::chat;
use super::credentials::Credential;
use super::models;
use super::session;
use super::{accounts, balance, login, onboarding, refresh, welfare};

/// provider id（落盘契约，存量 id 不能改名）。
pub const PROVIDER_ID: &str = "codearts";

/// 进程级本地准入闸（同一上游身份的并发会话共享一个上限）。
static SESSION_GATE: OnceLock<session::SessionGate> = OnceLock::new();

/// CodeArts 适配器（无状态字段：目录缓存在 `models` 里，凭据在账号存储里）。
#[derive(Clone, Copy)]
pub struct CodeArtsProvider;

/// 静态实例：注册表要的是 `&'static dyn Provider` 的等价物。
pub static CODEARTS: CodeArtsProvider = CodeArtsProvider;

impl Provider for CodeArtsProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(PROVIDER_ID, "CodeArts")
            .with_capabilities(proto::Capabilities {
                chat: true,
                // 会话心跳 + 每账号 3 路并发 + 首包门（见模块头）
                stateful: true,
                // refresh_token 一次性，写回走 `store::save`（见 `refresh.rs`）
                refresh: true,
                // 三源目录发现（`models.rs`）
                model_refresh: true,
                // 两份账：订阅统计 + 福利网关（`balance.rs`）
                usage: true,
                // 每日 ops 福利 = 本家的「签到」（`welfare.rs`）
                checkin: true,
                // 新人注册礼（`onboarding.rs`）
                claims: true,
                // OAuth 授权码 + PKCE + DPoP（`login.rs`）
                web_login: true,
                // 上游已经是 OpenAI chunk 形状，透传即可
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                // 没有环境变量旁路：临时凭据必须配着 oauth_context 才成立
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                refresh_uses_account: true,
                // 本家是否认「默认模型」概念（协议新增位，见 `Capabilities`）
                supports_default_model: false,
                // 本家有没有环境变量 / 默认登录态旁路（协议新增位）
                env_credentials: false,
            })
            .with_login(vec![
                proto::LoginMethod::Browser {
                    button: "网页登录".to_string(),
                    hint: Some("打开华为云授权页，完成后自动收下账号".to_string()),
                },
                proto::LoginMethod::Token {
                    label: "登录凭据".to_string(),
                    placeholder: "粘贴 access_key_id / secret_access_key（或整份 auth 文件内容）"
                        .to_string(),
                    extra_fields: Vec::new(),
                },
            ])
            .with_models(models::list())
            // 账号行：各位各有出处，别照着别家抄。
            //   `usage` —— 余额是**两份账**（订阅统计 + 福利网关，见 `balance`），
            //     界面上「读到 0」与「没读到」必须能分开，后端因此把失败的一侧
            //     写进 `statisticsError` / `benefitError` 而不是整次失败。
            //   `usage_detail` —— 余额列走「套餐徽标 + 点击逐项读数」的两行形态：
            //     本家的计量表一行放不下，「可用 N 积分」是各池加总，两个权益
            //     不同的账号在列里看不出差别。
            //   `edition: false` —— 没有版本 / 地区概念：region 固定在 cn-north-4
            //     且必须与 token 签发地一致，不是用户可选项；`login_type`
            //     （WEB/IDE）也不是版本。
            //   `expiry: expiresAt` —— 临时凭据约一小时到期，这一列对本家
            //     **是主要信息**。
            //   `concurrency_default: 3` —— 上游硬顶（超过直接回 HTTP 400，且是
            //     账号级冲突、不降级换号），所以 0 解释成「按默认 3」。
            //   `default_limiters` —— 余额读数常判不出（免费版账号的统计里没有
            //     积分类计量表、`available` 为 null，额度全在福利 token 池），
            //     缺省的余额规则形同虚设；按自然日 Token 消耗判定才兜得住
            //     「别把一个账号打爆」，而「过了 0 点额度就回来」正是它的额度形态。
            .with_account_features(proto::AccountFeatures {
                usage: true,
                usage_detail: true,
                identifier: vec!["userId".to_string(), "uid".to_string()],
                expiry: vec!["expiresAt".to_string()],
                concurrency_default: Some(3),
                default_limiters: Some(vec![proto::DefaultLimiter {
                    kind: proto::DefaultLimiterKind::Token,
                    threshold: 10_000_000.0,
                    period_seconds: 0,
                    action: proto::DefaultLimiterAction::Skip,
                }]),
                ..Default::default()
            })
        .with_checkin(proto::CheckinInfo {
            description: Some(
                "每日福利 · 领取 → 确认 → 回读核实 · 按北京时间自然日重置".to_string(),
            ),
            modes: vec![proto::CheckinMode::Checkin],
            onboarding: true,
            ..Default::default()
        })
    }

    /// 清单来自刷新链路落的进程内缓存（`models::list` 是同步读）。
    fn models<'a>(&'a self, _ctx: &'a PluginCtx, _account: &'a str) -> SendFuture<'a, proto::ModelList> {
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

    /// **会话式转发入口**：占槽 → 签名发送 → 首包门 → 透传/折叠。
    ///
    /// ── 顺序是有意的 ────────────────────────────────────────────
    /// **先过本地准入再占上游会话**。反过来的话，一个满员的账号会白占一个上游
    /// 槽位并立刻释放，把并发闸变成噪声。
    ///
    /// ── 两条收尾路径都必须走到 `ChatSession::stop()` ────────────
    /// 中途 `?` 提前返回时由 `ChatSession` 的 `Drop` 发注销信号，但那条不等
    /// idle 落地 —— 所以出错分支上显式 `stop().await`。
    fn conversation<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ConversationRequest,
        sink: ConversationSink<'a>,
    ) -> SendFuture<'a, ()> {
        Box::pin(async move {
            let mut sink = sink;
            let account_id = req.account.clone();
            if account_id.trim().is_empty() {
                return Err("没有可用的 CodeArts 账号：请在账号页添加并启用账号".to_string());
            }
            // ① 凭据（含临期主动续期与写回）；代理沿用编排层为本账号解析出的那份
            let credential = refresh::ensure_fresh(ctx, &account_id, false, req.proxy.as_ref())
                .await
                .map_err(GatewayError::into_message)?;
            if !credential.valid() {
                return Err("CodeArts 账号缺少可用的临时凭据，请重新登录".to_string());
            }

            // ② 模型名归一：客户端习惯小写，上游要真名；福利模型要带 maas_type
            let requested = req
                .body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let catalog = models::cached_catalog().ok_or_else(|| {
                "CodeArts 模型目录还没拉取过：请先在模型页对该账号执行一次「获取模型」".to_string()
            })?;
            let Some(model) = catalog.resolve(&requested) else {
                return Err(models::unknown_model_error(&requested, &catalog).message);
            };
            let benefit = model.source.needs_benefit_header();
            let upstream_model = model.id.clone();
            let max_output_tokens = model.max_output_tokens;

            // ②.5 尺寸门：这一家刚在「体 ≥ N 字节」上被上游拒过（TTL 内），同尺寸的
            // 就别再发出去 —— 上游那道墙按字节判，重发同一份体结论不变。
            //
            // 估的是客户端 body 的字节数（**只在有门时**才付这一次序列化），比真实
            // 发出量小（本家还要塞会话 id、可能抬 max_tokens），所以留 64 KiB 余量。
            const ESTIMATE_SLACK_BYTES: i64 = 64 * 1024;
            let now_ms = logging::now_ms();
            if let Some(floor) = super::size_gate::floor(now_ms) {
                let estimate = serde_json::to_string(&req.body)
                    .map(|text| text.len() as i64)
                    .unwrap_or_default();
                if estimate + ESTIMATE_SLACK_BYTES >= floor {
                    return Err(format!(
                        "CodeArts 发不出这么大的请求体（本次约 {} 字节，这一家已知 ≥ {} 字节会被上游拒）：\
                         本轮绕开这一家，最长 {} 秒后会重新探一次；\
                         要立刻可用就减小上下文（少带历史 / 别整份贴文件）",
                        estimate,
                        floor,
                        super::size_gate::ms_left(now_ms) / 1000
                    ));
                }
            }

            // ③ 本地准入（满员回 409，不冷却账号）。
            // 身份优先用凭据里的 domain+user；两个都取不到时退到**账号行 id**
            // （不是 AK —— 它每次续期都换，用它当键等于每刷一次期就把并发上限清零）。
            let gate_identity = if credential.domain_id.trim().is_empty()
                && credential.user_id.trim().is_empty()
            {
                format!("account:{account_id}")
            } else {
                credential.identity()
            };
            // 面板那颗「并发上限」旋钮写的是账号上的 `maxConcurrent`。插件读不到
            // 记录的普通字段（只有 secret），所以用**账号 secret 里的同名键**——
            // 见 `account_view` 与 `accounts::into_account` 的说明；没写就用默认 3。
            let gate_override = account_max_concurrent(ctx, &account_id).await;
            let permit = SESSION_GATE
                .get_or_init(|| session::SessionGate::new(session::DEFAULT_SESSION_LIMIT))
                .acquire(models::DEFAULT_BASE_URL, &gate_identity, gate_override)
                .map_err(GatewayError::into_message)?;

            // ④ 占一个上游会话槽（心跳 busy + 后台续期）
            let mut options = session::SessionOptions::new(
                models::DEFAULT_BASE_URL,
                &credential,
                chat::DEFAULT_LANGUAGE,
            );
            options.interval = session::HEARTBEAT_INTERVAL;
            let session = match session::ChatSession::begin(options).await {
                Ok(session) => session,
                Err(error) => {
                    drop(permit);
                    return Err(error.message);
                }
            };
            let session_id = session.id().to_string();

            // ⑤ 构造并发出（签名要覆盖到刚生成的 session id，所以顺序在后）
            let profile = chat::HeaderProfile {
                chat_session_id: Some(session_id),
                ..Default::default()
            };
            let (url, headers, payload, restorer) = {
                // 客户端额度太小、而这个模型一定先思考时，抬到 +预留：不抬的话上游
                // 会把额度全花在 reasoning 上，客户端收到一次"成功的空回答"。
                let mut outbound = req.body.clone();
                if let Some((from, to)) = chat::reserve_for_thinking(&mut outbound, max_output_tokens) {
                    logging::verbose(
                        "[CodeArts]",
                        &format!(
                            "客户端 max_tokens {from} 撑不下这个模型的思考，抬到 {to}（该模型上限 {}）",
                            if max_output_tokens > 0 {
                                max_output_tokens.to_string()
                            } else {
                                "未声明".to_string()
                            }
                        ),
                    );
                }
                // 函数名超上限的那道改写也必须发生在签名之前：改名动了 `tools`，
                // 而签名覆盖的就是出门那串字节。
                let restorer = super::tool_names::shorten(&mut outbound);
                match chat::build_upstream_request(
                    models::DEFAULT_BASE_URL,
                    &upstream_model,
                    outbound,
                    true,
                    benefit,
                    &profile,
                    Some(&credential),
                ) {
                    Ok((url, headers, payload)) => (url, headers, payload, restorer),
                    Err(error) => {
                        session.stop().await;
                        drop(permit);
                        return Err(error.message);
                    }
                }
            };
            let mut request = crate::http::client(req.proxy.as_ref(), "direct").post(&url);
            for (name, value) in headers {
                request = request.header(name.as_str(), value.as_str());
            }
            // 真实发出量：只有这里知道（上游那道墙按它判）
            let wire_bytes = payload.len() as i64;
            let response = match request.body(payload).send().await {
                Ok(response) => response,
                Err(error) => {
                    session.stop().await;
                    drop(permit);
                    return Err(format!(
                        "CodeArts 对话请求失败：{}",
                        crate::http::describe_chain(&error)
                    ));
                }
            };

            // ⑥ 非 2xx：原样带状态码 + 脱敏诊断体
            let status = response.status().as_u16();
            if !(200..300).contains(&status) {
                let (error_body, truncated) = chat::read_error_body(response).await;
                let error_text = String::from_utf8_lossy(&error_body).to_string();
                let size_reason = chat::size_rejection_any(status, &error_text, wire_bytes);
                if matches!(status, 400 | 413 | 502) && size_reason.is_none() {
                    let preview: String = error_text.chars().take(200).collect();
                    logging::verbose(
                        "[CodeArts]",
                        &format!("非 2xx 诊断体（尺寸判据未命中，前 200 字）：{preview}"),
                    );
                }
                note_size_gate(size_reason, wire_bytes);
                session.stop().await;
                drop(permit);
                let error = chat::upstream_http_error(status, &error_body, &credential, truncated);
                return Err(error.message);
            }

            // ⑦ 首包门：一个字节都没下发之前就决定"交出去"还是"换账号"
            let (prefetched, rest) = match chat::prefetch_head(response).await {
                Ok(head) => head,
                Err(error) => {
                    note_size_gate(
                        chat::size_rejection_any(0, &error.message, wire_bytes),
                        wire_bytes,
                    );
                    session.stop().await;
                    drop(permit);
                    return Err(error.message);
                }
            };
            // ⑦.5 体积墙也走在**流里**：HTTP 200 + 一帧 `InferHub.001001005.400`
            // 才是真结论。尺寸类失败必然出现在第一帧，所以扫头部字节就够。
            note_size_gate(
                chat::size_rejection_any(0, &String::from_utf8_lossy(&prefetched), wire_bytes),
                wire_bytes,
            );

            if !req.stream {
                // 非流式：把剩下的读完再折叠（上游只有流式）
                use futures::StreamExt;
                let mut all = prefetched;
                let mut rest = rest;
                let read_error = loop {
                    match rest.next().await {
                        None => break None,
                        Some(Err(error)) => break Some(error),
                        Some(Ok(chunk)) => all.extend_from_slice(&chunk),
                    }
                };
                session.stop().await;
                drop(permit);
                if let Some(error) = read_error {
                    return Err(format!("CodeArts 上游流中断：{error}"));
                }
                note_size_gate(
                    chat::size_rejection_any(0, &String::from_utf8_lossy(&all), wire_bytes),
                    wire_bytes,
                );
                let mut completion = chat::aggregate_sse(&all, &upstream_model)
                    .map_err(GatewayError::into_message)?;
                if let Some(restorer) = restorer.as_ref() {
                    restorer.apply_completion(&mut completion);
                }
                let payload = serde_json::to_string(&completion).map_err(|error| {
                    format!("CodeArts 聚合结果序列化失败：{error}")
                })?;
                sink.head(proto::StreamHead {
                    status: 200,
                    headers: [("content-type".to_string(), "application/json".to_string())]
                        .into_iter()
                        .collect(),
                    upstream: proto::UpstreamKind::Chat,
                })
                .await
                .map_err(|error| format!("客户端已断开：{error}"))?;
                sink.send(payload.as_bytes())
                    .await
                    .map_err(|error| format!("客户端已断开：{error}"))?;
                return Ok(());
            }

            // ⑧ 流式：透传（上游已是 OpenAI chunk 形状），结束时释放会话与许可
            sink.head(proto::StreamHead {
                status: 200,
                headers: [("content-type".to_string(), "text/event-stream".to_string())]
                    .into_iter()
                    .collect(),
                upstream: proto::UpstreamKind::Chat,
            })
            .await
            .map_err(|error| format!("客户端已断开：{error}"))?;
            use futures::StreamExt;
            // 改过函数名才需要还原响应里的名字；没改过时这个 fixer 全程 idle，
            // 一个字节都不碰，走的还是原样透传。
            let mut fixer = super::tool_names::StreamFixer::new(restorer);
            let mut sniffer = chat::UsageSniffer::default();
            let sniff_telemetry = std::sync::Arc::new(RequestTelemetry::new());
            if !prefetched.is_empty() {
                sniffer.feed(&prefetched, &sniff_telemetry);
                let head = if fixer.idle() {
                    prefetched.clone()
                } else {
                    fixer.push(&prefetched).into_owned()
                };
                if sink.send(&head).await.is_err() {
                    session.stop().await;
                    drop(permit);
                    return Ok(());
                }
            }
            let mut rest = rest;
            while let Some(item) = rest.next().await {
                if let Ok(bytes) = item.as_ref() {
                    sniffer.feed(bytes, &sniff_telemetry);
                }
                let to_send = if fixer.idle() {
                    match item {
                        Ok(bytes) => bytes.to_vec(),
                        Err(error) => {
                            // 上游流断了：先收摊，再把错误如实交给调用方
                            session.stop().await;
                            drop(permit);
                            return Err(format!("CodeArts 上游流中断：{error}"));
                        }
                    }
                } else {
                    match item {
                        Ok(bytes) => fixer.push(&bytes).into_owned(),
                        Err(error) => {
                            session.stop().await;
                            drop(permit);
                            return Err(format!("CodeArts 上游流中断：{error}"));
                        }
                    }
                };
                if sink.send(&to_send).await.is_err() {
                    // 客户端断开：idle 必须发，否则上游槽位悬着
                    break;
                }
            }
            // 流结束时把成帧缓冲里剩下的一段交出去（半条事件还原不了也照原样放行）
            let tail = fixer.finish();
            if !tail.is_empty() {
                let _ = sink.send(&tail).await;
            }
            sniffer.finish(&sniff_telemetry);
            session.stop().await;
            drop(permit);
            Ok(())
        })
    }

    /// 非 2xx 的分类。
    ///
    /// **注意**：CodeArts 的业务失败常常不体现在状态码上（额度耗尽是 HTTP 200
    /// SSE 里的信封），那部分由首包门 + `stream_fault` 负责；这里只管真的是
    /// 状态码的错。三条口径与参考实现一致：403 = 额度、429 = 限流、其余透传。
    fn classify(&self, status: u16, error_body: &Value) -> proto::ErrorClass {
        let raw = error_body
            .get("message")
            .or_else(|| error_body.get("error_msg"))
            .or_else(|| error_body.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let message = format!("上游返回 {status}{}", raw_part(raw));
        match status {
            // 403：额度耗尽 —— 标记账号冷却并换下一账号。恢复时刻只在**认得出
            // 日池**时给（下一个北京零点），其余仍走存储层的 10 分钟兜底。
            403 => proto::ErrorClass::QuotaLimited {
                reset_at: daily_pool_reset_at(logging::now_ms(), &message),
                message,
                upstream_code: None,
                status,
            },
            429 => proto::ErrorClass::QuotaLimited {
                // 429 是分钟级限流，与日池无关：不给它零点，交给 10 分钟兜底
                reset_at: None,
                message,
                upstream_code: None,
                status,
            },
            // 401：临时凭据过期/被换掉 —— 刷一次再用同一账号重试
            401 => proto::ErrorClass::TokenExpired { message },
            _ => proto::ErrorClass::Fatal {
                status,
                message,
                upstream_code: None,
            },
        }
    }

    /// 福利池是**账号级**日额度：本次失败的模型是福利源时，整组福利模型一起
    /// 记冷却 —— 不然池子已经空了，换一个福利模型名照样从头撞一遍。
    fn cooldown_models(&self, _account: &str, wire_model: &str) -> proto::CooldownModels {
        match models::cached_catalog() {
            Some(catalog) => proto::CooldownModels {
                models: benefit_cooldown_group(&catalog, wire_model),
            },
            None => proto::CooldownModels::default(),
        }
    }

    /// 取一个**可用**的 access key：临期就真刷并写回。
    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let proxy = record_proxy(ctx, account).await?;
            Ok(refresh::ensure_fresh(ctx, account, false, proxy.as_ref())
                .await
                .map_err(GatewayError::into_message)?
                .access_key_id)
        })
    }

    /// 面板上那条「刷新凭据」走的是这条（不看临期窗口）。
    ///
    /// **不能省**：漏了它就落到 trait 的默认实现（= `ensure_token`），用户点了
    /// 刷新、看着成功，临期凭据照旧临期。这类「报成功但什么都没发生」比报错更难查。
    fn refresh_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let proxy = record_proxy(ctx, account).await?;
            Ok(refresh::ensure_fresh(ctx, account, true, proxy.as_ref())
                .await
                .map_err(GatewayError::into_message)?
                .access_key_id)
        })
    }

    /// 临期判定要与 `ensure_fresh` 用同一个提前量：批量维护循环据此决定要不要动
    /// 这个账号，回 false 就等于把它从维护范围里永久剔除（本家凭据默认只活一小时）。
    fn token_expiring<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, bool> {
        Box::pin(async move {
            if account.trim().is_empty() {
                return Ok(false);
            }
            let record = match crate::store::record(ctx, PROVIDER_ID, account).await {
                Ok(record) if record.is_object() => record,
                _ => return Ok(false),
            };
            Ok(Credential::from_payload(&record)
                .map(|credential| {
                    credential.can_refresh()
                        && credential.needs_refresh(refresh::REFRESH_LEAD_MS, logging::now_ms())
                })
                .unwrap_or(false))
        })
    }

    /// 余额可查（两份账，见 `balance.rs` 模块头：订阅统计 + 福利网关）。
    fn usage<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, Value> {
        Box::pin(async move { query_usage(ctx, account).await.map_err(GatewayError::into_message) })
    }

    /// 目录刷新：三源发现 → 落进程内缓存。
    ///
    /// 目录本身只读，但**取凭据要走 `ensure_fresh`**：本家的临时凭据只活一小时
    /// 左右，用户很可能在闲置很久之后才第一次点「获取模型」—— 直接用盘上那份
    /// 会撞 401，而 401 在目录这条路上只会显示成"刷新失败"。
    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        _force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            // 空 id = 队首可用账号（自动路径的默认）；非空 = 用户在弹窗里点名的
            // 那条。**「没有账号」不是失败**：自动路径每轮都会走到这里，把它算成
            // 失败会给一家用户还没配置的家记上一次失败、进而触发冷却退避。
            let record = crate::store::record(ctx, PROVIDER_ID, account).await?;
            if !record.is_object() {
                logging::verbose(
                    "[Models]",
                    "CodeArts 模型目录刷新跳过：尚未添加 CodeArts 账号",
                );
                if account.trim().is_empty() {
                    return Ok(proto::RefreshOutcome::unchanged());
                }
                return Ok(proto::RefreshOutcome::failed(
                    "指定的 CodeArts 账号不存在或不可用，请重新选择",
                ));
            }
            let proxy = match record_proxy(ctx, account).await {
                Ok(proxy) => proxy,
                Err(error) => return Ok(proto::RefreshOutcome::failed(error.message)),
            };
            let credential = match refresh::ensure_fresh(ctx, account, false, proxy.as_ref()).await {
                Ok(credential) => credential,
                Err(error) => return Ok(proto::RefreshOutcome::failed(error.message)),
            };
            let endpoints = models::CatalogEndpoints {
                base_url: models::DEFAULT_BASE_URL,
                benefit_gateway_url: Some(models::DEFAULT_BENEFIT_GATEWAY_URL),
                plugin_version: chat::DEFAULT_PLUGIN_VERSION,
                language: chat::DEFAULT_LANGUAGE,
            };
            let catalog = models::discover(&endpoints, &credential).await;
            if catalog.models.is_empty() {
                return Ok(proto::RefreshOutcome::failed(
                    catalog
                        .warnings
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "该账号没有返回任何可用模型".to_string()),
                ));
            }
            let count = catalog.models.len();
            models::store_catalog(catalog);
            Ok(proto::RefreshOutcome::refreshed(count))
        })
    }

    // ── 签到 / 领取 ───────────────────────────────────────────

    /// 每日签到 = 本家的「每日登录送积分」那一组 ops 福利（`welfare.rs`）。
    ///
    /// `manual` 那个维度插件协议里没有：`checkin.claim` 只在「用户点了签到」与
    /// 「定时任务」两条路上被调，而后者在本家**本来就不接**（见网关
    /// `auto_checkin::CHECKIN_PROVIDERS` 与本家 `welfare` 的模块头）。所以这里
    /// 按 `manual = true` 走 —— 本地限流闸（每天 ≤6 次写 / 间隔 ≥10 分钟）
    /// 保护的是无人值守的重试，不是用户点击。
    fn checkin<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(async move {
            let outcome = welfare::claim_account(
                ctx,
                account,
                models::DEFAULT_BASE_URL,
                logging::now_ms(),
                true,
            )
            .await
            .map_err(GatewayError::into_message)?;
            Ok(welfare_report(outcome))
        })
    }

    /// 新手任务状态（新人注册礼，`onboarding.rs`）。
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

    /// 一键领取（`claim` 为空串 = 全部可领项；本家只有新人注册礼一条）。
    fn claim<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str, _claim: &'a str) -> SendFuture<'a, Value> {
        Box::pin(async move {
            onboarding::claim_all(ctx, account)
                .await
                .map_err(GatewayError::into_message)
        })
    }

    /// 管理动作：每日福利的「预读 / 领取」。
    ///
    /// ```text
    ///   welfare.preview → GET /v1/ops/delivery（只读，一个写请求都不发）
    ///   welfare.claim   → 真的去领（payload.auto=true 时走 6 次/10 分钟的写闸）
    /// ```
    ///
    /// 领完顺手把余额读回来：面板上「领到了」与「积分确实变了」是两件事，
    /// 一次点击能同时回答最好（余额读失败给 null，不影响领取结果）。
    fn admin_action<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        action: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            let base = super::models::DEFAULT_BASE_URL;
            let now_ms = logging::now_ms();
            match action {
                "welfare.preview" => super::welfare::preview(ctx, account, base, now_ms)
                    .await
                    .map_err(GatewayError::into_message),
                "welfare.claim" => {
                    // 缺省必须是 **false（手动）**：限流两道闸存在的理由是「别在
                    // 没人看着的时候反复花掉领取机会」。非布尔也按手动处理 ——
                    // 认字符串会让手滑的调用方绕过限流。
                    let auto = payload.get("auto").and_then(Value::as_bool).unwrap_or(false);
                    let outcome =
                        super::welfare::claim_account(ctx, account, base, now_ms, !auto)
                            .await
                            .map_err(GatewayError::into_message)?;
                    let usage = super::welfare::refresh_usage(ctx, account, now_ms).await;
                    Ok(serde_json::json!({
                        "result": outcome.label(),
                        "confirmed": matches!(outcome, super::welfare::Outcome::Confirmed),
                        "usage": usage,
                        "note": "领取到账的是套餐赠送积分，不增加福利模型 token 池",
                    }))
                }
                other => Err(format!(
                    "CodeArts 不认识管理动作「{other}」（可用：welfare.preview / welfare.claim）"
                )),
            }
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    fn normalize_account<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(accounts::normalize(input))
    }

    fn account_view(&self, secret: &Value) -> Result<Value, String> {
        Ok(accounts::account_view(secret))
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(accounts::identity_of(secret))
    }

    // ── 登录（OAuth 授权码 + PKCE + DPoP）──────────────────────

    fn login_start<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        method: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(login::login_start(method, payload))
    }

    fn login_poll<'a>(&'a self, _ctx: &'a PluginCtx, flow: &'a str) -> SendFuture<'a, proto::LoginPoll> {
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

/// 登记尺寸门（体积墙可能走在 HTTP 状态上，也可能走在 SSE 第一帧里，三处出口共用）。
///
/// 只在**从放行变成挡住**那一次打一行终端日志：门存续期内每发大请求都会命中
/// 判据，逐请求打就是刷屏。
fn note_size_gate(reason: Option<&'static str>, wire_bytes: i64) {
    let Some(reason) = reason else { return };
    if super::size_gate::hold(wire_bytes, logging::now_ms()) {
        logging::console_line(
            "[Routing]",
            &format!(
                "⚠️ CodeArts 的尺寸门已登记：{}（本次发出 {} 字节），同尺寸或更大的请求暂时绕开这一家",
                reason, wire_bytes
            ),
        );
    }
}

/// 纯函数：给定目录与本次真名，返回整组冷却名单。福利判定**大小写无关**
/// （客户端的大小写变体也算福利）；命中 → 全部福利模型 id，并把**本次发送名
/// 原文**也补进名单。
fn benefit_cooldown_group(catalog: &models::Catalog, wire_model: &str) -> Vec<String> {
    let is_benefit = catalog.models.iter().any(|model| {
        model.id.eq_ignore_ascii_case(wire_model) && model.source == models::ModelSource::Benefit
    });
    if !is_benefit {
        return vec![wire_model.to_string()];
    }
    let mut names: Vec<String> = catalog
        .models
        .iter()
        .filter(|model| model.source == models::ModelSource::Benefit)
        .map(|model| model.id.clone())
        .collect();
    if !names.iter().any(|name| name == wire_model) {
        names.push(wire_model.to_string());
    }
    names
}

/// 账号自己配的代理，口径与 qoder 的 `auth::account_proxy` 一致：
/// **解析失败就报错**，不静默降级成直连。
pub(crate) async fn record_proxy(ctx: &PluginCtx, account_id: &str) -> Result<Option<ResolvedProxy>, GatewayError> {
    let record = crate::store::record(ctx, PROVIDER_ID, account_id).await?;
    if !record.is_object() {
        return Err(GatewayError::with_status(
            503,
            "没有可用的 CodeArts 账号：请在账号页添加并启用账号",
        ));
    }
    match resolve_account_proxy(record.get("proxy")) {
        Some(ProxyResolution::Resolved(proxy)) => Ok(Some(proxy)),
        Some(ProxyResolution::Failed(reason)) => Err(GatewayError::with_status(400, reason)),
        None => Ok(None),
    }
}

/// 面板上「并发上限」那颗旋钮：插件读不到账号记录的普通字段，只读 secret 里的
/// 同名键（`maxConcurrent`）。0 / 缺省 = 继承默认（见 `session::SessionGate`）。
async fn account_max_concurrent(ctx: &PluginCtx, account_id: &str) -> Option<u64> {
    let record = crate::store::record(ctx, PROVIDER_ID, account_id).await.ok()?;
    record
        .get("maxConcurrent")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
}

/// 余额查询（两份账：订阅统计 + 福利网关）。
///
/// **两个源各自成功各自失败**：福利网关是另一台机器，它挂了不该让订阅统计
/// 显示不出来（反过来也是）。只有两边都失败才整次失败 —— 而那通常意味着
/// 凭据本身已经不行了，错误文案照 `statistics` 那侧给（它才是主账）。
async fn query_usage(ctx: &PluginCtx, account_id: &str) -> Result<Value, GatewayError> {
    let credential = welfare::current_credential(ctx, account_id).await?;
    let (statistics, benefit) = balance::fetch_both(
        models::DEFAULT_BASE_URL,
        models::DEFAULT_BENEFIT_GATEWAY_URL,
        &credential,
        chat::DEFAULT_LANGUAGE,
        chat::DEFAULT_PLUGIN_VERSION,
    )
    .await;
    // ── 「无 benefit 档案」先自动注册一次再重查 ─────────────────
    // benefit 记录不是开账号就有的：官方客户端每次启动都 POST claim（幂等建档），
    // 没走过这一步的账号 InferHub 一律 `4004.200 benefit not found` 拒答。
    // 去抖：注册是写语义的幂等调用，没必要每分钟补一发（10 分钟一次足够）。
    let mut benefit = benefit;
    if matches!(&benefit, Ok(None)) && benefit_claim_due(account_id) {
        match balance::claim_benefit(models::DEFAULT_BENEFIT_GATEWAY_URL, &credential).await {
            Ok(_) => {
                benefit = balance::fetch_benefit_balance(
                    models::DEFAULT_BENEFIT_GATEWAY_URL,
                    &credential,
                )
                .await;
            }
            Err(error) => {
                logging::verbose(
                    "[CodeArts]",
                    &format!("benefit 自动注册失败（账号 {account_id}）：{}", error.message),
                );
            }
        }
    }
    if statistics.is_err() && benefit.is_err() {
        let (first, second) = (statistics.as_ref().err(), benefit.as_ref().err());
        let message = match (first, second) {
            (Some(first), Some(second)) => {
                format!("{}（福利网关：{}）", first.message, second.message)
            }
            (Some(first), _) => first.message.clone(),
            (_, Some(second)) => second.message.clone(),
            _ => "CodeArts 余额查询失败".to_string(),
        };
        let status = statistics
            .as_ref()
            .err()
            .map(|error| error.status_code)
            .unwrap_or(502);
        return Err(GatewayError::with_status(status, message));
    }
    let benefit_value = benefit.as_ref().ok().and_then(|value| value.as_ref());
    let mut document = balance::usage_document(statistics.as_ref().ok(), benefit_value);
    match &benefit {
        // 上游明说没有该账号的福利数据：落一个**中性**标记而不是错误
        Ok(None) => {
            document["benefitAbsent"] = Value::Bool(true);
        }
        Ok(Some(_)) => {}
        Err(error) => {
            document["benefitError"] = Value::String(error.message.clone());
        }
    }
    if let Err(error) = &statistics {
        document["statisticsError"] = Value::String(error.message.clone());
    }
    Ok(document)
}

/// benefit 自动注册（claim）的**进程级去抖**：距上次尝试不足窗口期返回 false。
///
/// 自动余额查询默认每分钟跑一次，「无档案」的账号若不加去抖就会每分钟被补发
/// 一次 claim —— 调用幂等但没必要（官方客户端也就是每次启动一发）。
fn benefit_claim_due(account_id: &str) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    const WINDOW: Duration = Duration::from_secs(600);
    static LAST: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    let map = LAST.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = match map.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.get(account_id).is_some_and(|at| at.elapsed() < WINDOW) {
        return false;
    }
    guard.insert(account_id.to_string(), Instant::now());
    true
}

/// 把上游原文拼成统一文案的一截（空则不拼）。
fn raw_part(raw: &str) -> String {
    if raw.trim().is_empty() {
        String::new()
    } else {
        format!(": {}", raw.trim())
    }
}

/// 认得出「福利日池打满」的文案 ⇒ 冷却到**北京时间下一个零点**；认不出 ⇒ None
/// （由存储层落 10 分钟兜底）。
///
/// `InferHub.4291.200 / insufficient quota` 是本家**按日**的 token 池打满时那一帧。
/// 上游不告诉我们什么时候重置，但它整套日口径都是 UTC+8 零点（见
/// `welfare::today` 的说明），所以零点是有据可依的那个答案。
///
/// 只认这两串原文，不认状态码：429 是限流（分钟级，10 分钟兜底正是它要的），
/// 认不出额度的 403 是内容闸门/权限/签名那一类（本来就不该罚账号）。
fn daily_pool_reset_at(now_ms: i64, message: &str) -> Option<i64> {
    let text = message.to_lowercase();
    if text.contains("insufficient quota") || text.contains("4291.200") {
        Some(welfare::next_day_boundary_ms(now_ms))
    } else {
        None
    }
}

/// `welfare::Outcome` → `CheckinReport`。
///
/// 四档结论在「今天还能不能再领」这件事上分三类，映射必须与网关的
/// `checkin_completed_today` / `claim_pending` 口径对齐：
///   · `Confirmed`（官方已确认到账）→ `Claimed`：今天真领到了；
///   · `Already`（已领取并确认）→ `Done`：上游说今天已经领过；
///   · `NotEligible` / `Skipped` → `Inactive`：暂无可领取活动（中性未领取）。
fn welfare_report(outcome: welfare::Outcome) -> proto::CheckinReport {
    use proto::CheckinOutcome;
    let label = outcome.label().to_string();
    match outcome {
        welfare::Outcome::Confirmed => proto::CheckinReport {
            outcome: CheckinOutcome::Claimed,
            credit: None,
            streak: None,
            message: Some(label),
        },
        welfare::Outcome::Already => proto::CheckinReport {
            outcome: CheckinOutcome::Done,
            credit: None,
            streak: None,
            message: Some(label),
        },
        welfare::Outcome::NotEligible | welfare::Outcome::Skipped => proto::CheckinReport {
            outcome: CheckinOutcome::Inactive,
            credit: None,
            streak: None,
            message: Some(label),
        },
    }
}
