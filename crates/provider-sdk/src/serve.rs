//! `serve()`：插件的进程主循环。
//!
//! ── 结构（改之前务必读完）────────────────────────────────────
//! ```text
//! ┌── 读循环（stdin）───────────────────────────────┐
//! │ 读到请求 → 起一个 task 处理（不等它）            │
//! │ 读到应答 → 唤醒对应的 oneshot（回调的返回）      │
//! │ EOF      → 退出                                  │
//! └──────────────────────────────────────────────────┘
//!           │ 业务 task 需要写 stdout
//!           ▼
//! ┌── 写队列（一把 tokio Mutex 包住 stdout）────────┐
//! │ 每帧写完立即 flush；同一条流的帧顺序天然保持     │
//! └──────────────────────────────────────────────────┘
//! ```
//!
//! ── 为什么读循环绝不在里面 await 业务 ────────────────────────
//! 业务代码会回调网关（`store.get`）。那条回调的应答必须由读循环派发；
//! 若读循环正卡在等业务完成，应答永远读不到 —— 自己把自己锁死。
//! 所以读循环只做「分类 + 投递」，一条业务 await 都没有。
//!
//! ── 背压为什么在这里实现 ────────────────────────────────────
//! 作者用 `sink.send(bytes).await` 推数据。这个 await 在窗口用完时挂起，
//! 直到网关回 `credit`。于是：客户端读得慢 → 网关不还额度 → 作者停在
//! `send` → 上游 TCP 窗口收缩。整条链路不需要任何一处主动轮询。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, OnceLock};

use agent2api_provider_protocol as proto;
use serde_json::Value;
use tokio::sync::{oneshot, Mutex, RwLock, Semaphore};

use crate::ctx::{CallGateway, PluginCtx, StoreError};
pub use crate::errors::{control_error_with_sign_in, control_error_with_timing};
use crate::provider::{BoxProvider, ConversationSink, Provider, SinkInner};
pub use crate::serve_options::{serve_blocking, serve_blocking_with, ServeOptions};

/// 全局宿主句柄（供 `log_info!` 这类宏在任意位置打日志）。
static HOST: OnceLock<Arc<Serve>> = OnceLock::new();

/// 启动插件主循环（在 tokio 运行时里调用）。
///
/// 正常退出（stdin EOF 或收到 `shutdown`）返回 `Ok(())`。
pub async fn serve(providers: Vec<BoxProvider>) -> std::io::Result<()> {
    serve_with(providers, ServeOptions::default()).await
}

pub async fn serve_with(providers: Vec<BoxProvider>, options: ServeOptions) -> std::io::Result<()> {
    let host = Arc::new(Serve::new(providers, options));
    let _ = HOST.set(host.clone());
    host.run().await
}

/// 打一条日志到网关（由 `log_info!` 等宏调用）。
pub fn emit_log(level: &str, message: String) {
    let Some(host) = HOST.get() else {
        // 还没启动：stderr 是唯一通道（正常不会走到这里）
        eprintln!("[{level}] {message}");
        return;
    };
    let host = host.clone();
    let level = level.to_string();
    let context = crate::log_context::current();
    // 不等待：日志是旁路，阻塞业务不值得。写失败也无所谓。
    tokio::spawn(async move {
        let frame = proto::Frame {
            id: 0,
            event: Some("log".into()),
            level: Some(level),
            message: Some(message),
            params: Some(context),
            ..Default::default()
        };
        host.write(frame).await.ok();
    });
}

/// 一条流式调用的取消信号。
///
/// ── 为什么需要它 ────────────────────────────────────────────
/// 客户端断开时网关发 `abort`。作者的 `conversation` 可能正卡在读上游，
/// 而不是卡在 `sink.send`。只让 `send` 返回错误的话，一个安静的上游能
/// 让这条调用多活几十秒。所以给作者一个可 select 的句柄
/// （`tokio::select! { _ = cancel.cancelled() => …, r = upstream.read() => … }`）。
#[derive(Debug, Default)]
pub struct Cancel {
    flag: AtomicBool,
    notify: tokio::sync::Notify,
}

impl Cancel {
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// 等到被取消（已经取消时立刻返回）。
    pub async fn cancelled(&self) {
        // 先登记等待者再读 flag，避免 notify_waiters 在两者之间发生而丢失唤醒。
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

/// 插件宿主本体。
pub(crate) struct Serve {
    /// 所有 provider（`Arc` 是为了让派发出去的 task 能持有）。
    providers: Vec<Arc<dyn Provider>>,
    /// 网关的写端。
    out: Mutex<tokio::io::Stdout>,
    /// 回调的应答等待者。
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<Value, proto::WireError>>>>,
    /// 下一个请求 id（0 保留给通知）。
    next_id: AtomicI64,
    /// 运行环境（`init` 时填）。
    env: RwLock<Option<proto::HostEnv>>,
    options: ServeOptions,
    /// 在途的流式调用：id → (取消信号, 额度池)。
    running: Mutex<HashMap<i64, Running>>,
    /// 正在关闭。
    closing: AtomicBool,
    cleanup: Mutex<Option<Result<(), String>>>,
}

/// 一条在途流式调用的状态。
struct Running {
    cancel: Arc<Cancel>,
    credit: Arc<Semaphore>,
}

impl Serve {
    fn new(providers: Vec<BoxProvider>, options: ServeOptions) -> Self {
        Self {
            providers: providers.into_iter().map(Arc::from).collect(),
            out: Mutex::new(tokio::io::stdout()),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
            env: RwLock::new(None),
            options,
            running: Mutex::new(HashMap::new()),
            closing: AtomicBool::new(false),
            cleanup: Mutex::new(None),
        }
    }

    async fn write(&self, frame: proto::Frame) -> std::io::Result<()> {
        let mut out = self.out.lock().await;
        proto::write_frame(&mut *out, &frame).await
    }

    async fn write_stream_frame(
        self: &Arc<Self>, frame: proto::Frame, cancel: Arc<Cancel>,
    ) -> std::io::Result<()> {
        // 取消前已开始的帧写完；尚未开始的帧跳过，不能迟到在 ABORTED 后面。
        let host = self.clone();
        tokio::spawn(async move {
            let mut out = host.out.lock().await;
            if cancel.is_cancelled() {
                return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "调用已被取消"));
            }
            proto::write_frame(&mut *out, &frame).await
        }).await.map_err(std::io::Error::other)?
    }

    /// 回一个成功应答。
    ///
    /// ── 写失败为什么要降级成 error 帧再试一次 ────────────────────
    /// 原先这里 `let _ = self.write(frame).await;` 把错误整个丢掉，于是
    /// 「结果帧太大写不出去」在插件侧完全静默 —— 网关等不到应答，最后
    /// 表现为一条超时，与真正的原因（帧超限）毫无关系（issue #15 的排查
    /// 成本主要就花在这上面）。
    ///
    /// 现在分两步：写成功就结束；写失败时**换一条 error 帧重试** —— 错误
    /// 帧只有一个短文案，体积恒定，几乎不可能再超限，于是网关能收到一句
    /// 「为什么失败」而不是干等到超时。两次都失败（管道真的断了）就只能
    /// 放弃：那种情况下对端已经不在了，再试也没有接收方。
    async fn reply(&self, id: i64, result: Value) {
        let frame = proto::Frame { id, result: Some(result), ..Default::default() };
        if let Err(error) = self.write(frame).await {
            let fallback = proto::Frame {
                id,
                error: Some(proto::WireError::with_code(
                    format!("插件无法回写应答：{error}"),
                    proto::WireError::INTERNAL,
                )),
                ..Default::default()
            };
            let _ = self.write(fallback).await;
        }
    }

    async fn reply_err(&self, id: i64, error: proto::WireError) {
        let frame = proto::Frame { id, error: Some(error), ..Default::default() };
        let _ = self.write(frame).await;
    }

    async fn shutdown_providers(&self) -> Result<(), String> {
        let mut cleanup = self.cleanup.lock().await;
        if let Some(result) = cleanup.as_ref() { return result.clone(); }
        self.closing.store(true, Ordering::Release);
        for running in self.running.lock().await.values() { running.cancel.cancel(); }
        let mut result = Ok(());
        for provider in &self.providers {
            if let Err(error) = provider.shutdown().await { if result.is_ok() { result = Err(error); } }
        }
        *cleanup = Some(result.clone());
        result
    }

    async fn run(self: &Arc<Self>) -> std::io::Result<()> {
        let stdin = tokio::io::stdin();
        let mut reader = proto::frame::buffered(stdin);
        loop {
            match proto::read_frame(&mut reader).await {
                Ok(Some(frame)) => match frame.kind() {
                    proto::FrameKind::Request => {
                        if self.closing.load(Ordering::Acquire) {
                            self.reply_err(
                                frame.id,
                                proto::WireError::with_code(
                                    "正在关闭",
                                    proto::WireError::INTERNAL,
                                ),
                            )
                            .await;
                        } else {
                            self.dispatch(frame);
                        }
                    }
                    proto::FrameKind::Reply | proto::FrameKind::Error => self.settle(frame).await,
                    // 网关不会给插件发流式中间帧；忽略而不是报错，
                    // 免得一个多余的帧把整个宿主判死。
                    proto::FrameKind::Event | proto::FrameKind::Unknown => {}
                },
                Ok(None) => break, // EOF：网关关了 stdin，正常退出
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                    // 帧坏了：不能信任后续字节边界，退出让网关重启我们
                    eprintln!("provider host: 帧格式错误，退出: {e}");
                    let _ = self.shutdown_providers().await;
                    return Err(e);
                }
                Err(e) => { let _ = self.shutdown_providers().await; return Err(e); }
            }
        }
        self.shutdown_providers().await.map_err(std::io::Error::other)
    }

    /// 派发一个请求到独立 task。
    fn dispatch(self: &Arc<Self>, frame: proto::Frame) {
        let Some(method) = frame.method.clone() else { return };
        let id = frame.id;
        let params = frame.params.clone().unwrap_or(Value::Null);
        let host = self.clone();

        // `abort` 与 `credit` 必须**立刻**处理，不能排队：
        // 它们的作用就是让正在跑的 task 让路，排队等于失效。
        if method == proto::methods::gateway::ABORT {
            tokio::spawn(async move {
                let target = params.get("id").and_then(Value::as_i64).unwrap_or(0);
                if let Some(running) = host.running.lock().await.get(&target) {
                    running.cancel.cancel();
                }
                host.reply(id, Value::Null).await;
            });
            return;
        }
        if method == proto::methods::gateway::CREDIT {
            tokio::spawn(async move {
                let target = params.get("id").and_then(Value::as_i64).unwrap_or(0);
                let n = params.get("n").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(running) = host.running.lock().await.get(&target) {
                    running.credit.add_permits(n);
                }
                host.reply(id, Value::Null).await;
            });
            return;
        }

        let context = crate::log_context::capture(id, &method, &params);
        let timeouts = serde_json::from_value::<crate::timeouts::Timeouts>(
            params.get("timeouts").cloned().unwrap_or(Value::Null),
        ).unwrap_or_default();
        tokio::spawn(async move {
            crate::timeouts::scope(
                timeouts,
                crate::log_context::scope(context, host.handle(id, &method, params)),
            ).await;
        });
    }

    /// 处理一个请求（在独立 task 里）。
    async fn handle(self: &Arc<Self>, id: i64, method: &str, params: Value) {
        match method {
            proto::methods::gateway::INIT => self.handle_init(id, params).await,
            proto::methods::gateway::PING => self.reply(id, Value::Null).await,
            proto::methods::gateway::SHUTDOWN => {
                match self.shutdown_providers().await {
                    Ok(()) => {
                        self.reply(id, Value::Null).await;
                        std::process::exit(0);
                    }
                    Err(message) => self.reply_err(id, crate::errors::to_wire_error(message)).await,
                }
            }
            proto::methods::gateway::CONVERSATION => self.handle_conversation(id, params).await,
            other => self.handle_control(id, other, params).await,
        }
    }

    async fn handle_init(self: &Arc<Self>, id: i64, params: Value) {
        let env = match serde_json::from_value::<proto::HostEnv>(
            params.get("env").cloned().unwrap_or(Value::Null),
        ) {
            Ok(env) => env,
            Err(e) => {
                self.reply_err(
                    id,
                    proto::WireError::with_code(
                        format!("init 的 env 无法解析: {e}"),
                        proto::WireError::BAD_PARAMS,
                    ),
                )
                .await;
                return;
            }
        };
        if let Some(on_init) = self.options.on_init {
            let initialized = tokio::task::spawn_blocking({
                let env = env.clone();
                move || on_init(&env)
            }).await;
            let error = match initialized {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(error) => Some(error.to_string()),
            };
            if let Some(error) = error {
                self.reply_err(id, proto::WireError::with_code(
                    format!("插件运行环境初始化失败: {error}"), proto::WireError::INTERNAL,
                )).await;
                return;
            }
        }
        *self.env.write().await = Some(env);
        let infos: Vec<proto::ProviderInfo> = self
            .providers
            .iter()
            .map(|provider| {
                let meta = provider.meta();
                // 拒绝标记在握手时一并申报（网关在每条流式响应上都要判，
                // 不能每次走 RPC —— 见协议里 ProviderInfo 的说明）。
                let (refusal_finishes, refusal_markers) = provider.refusal_markers();
                proto::ProviderInfo {
                    id: proto::ProviderId::new(meta.id),
                    label: meta.label,
                    order: meta.order,
                    capabilities: meta.capabilities,
                    login_methods: meta.login_methods,
                    default_models: meta.default_models,
                    needs_proxy: false,
                    refusal_finishes,
                    refusal_markers,
                    admin_panels: meta.admin_panels,
                    account_features: meta.account_features,
                    checkin: meta.checkin,
                }
            })
            .collect();
        match serde_json::to_value(&infos) {
            Ok(value) => self.reply(id, value).await,
            Err(e) => {
                self.reply_err(
                    id,
                    proto::WireError::with_code(
                        format!("provider 描述无法序列化: {e}"),
                        proto::WireError::INTERNAL,
                    ),
                )
                .await;
            }
        }
    }

    /// 控制面方法（全部一问一答）。
    async fn handle_control(self: &Arc<Self>, id: i64, method: &str, params: Value) {
        let Some(provider) = self.provider_of(&params) else {
            self.reply_err(
                id,
                proto::WireError::with_code(
                    "params.provider 缺失或未知",
                    proto::WireError::BAD_PARAMS,
                ),
            )
            .await;
            return;
        };
        let ctx = self.ctx(&provider.provider_id());
        let account = params.get("account").and_then(Value::as_str).unwrap_or("").to_string();

        let outcome: Result<Value, String> = match method {
            crate::provider::SCHEDULES_LIST_METHOD => {
                serde_json::to_value(provider.schedules())
                    .map_err(|error| format!("排程申报无法序列化: {error}"))
            }
            proto::methods::gateway::CHAT => {
                match serde_json::from_value::<proto::ChatRequest>(params.clone()) {
                    Ok(req) => match provider.build_chat(&ctx, req).await {
                        Ok(plan) => Ok(serde_json::json!({
                            "url": plan.url,
                            "headers": plan.headers,
                            "body": plan.body,
                            "upstream": plan.upstream,
                        })),
                        Err(e) => Err(e),
                    },
                    Err(e) => Err(format!("chat 参数无法解析: {e}")),
                }
            }
            crate::images::CAPABILITIES_METHOD => {
                Ok(serde_json::json!({
                    "generation": provider.supports_image_generation(),
                    "editing": provider.supports_image_editing(),
                }))
            }
            crate::images::GENERATE_METHOD | crate::images::EDIT_METHOD => {
                match serde_json::from_value::<crate::images::ImageRequest>(params.clone()) {
                    Ok(req) => {
                        let result = if method == crate::images::EDIT_METHOD {
                            provider.build_image_edit(&ctx, req).await
                        } else {
                            provider.build_image(&ctx, req).await
                        };
                        result.and_then(|plan| serde_json::to_value(plan)
                            .map_err(|error| format!("图片请求计划无法序列化: {error}")))
                    }
                    Err(error) => Err(crate::control_error(
                        400, format!("图片请求参数无法解析: {error}"), None, None,
                    )),
                }
            }
            proto::methods::gateway::CLASSIFY => {
                let status = params.get("status").and_then(Value::as_u64).unwrap_or(0) as u16;
                let body = params.get("body").cloned().unwrap_or(Value::Null);
                serde_json::to_value(provider.classify(status, &body))
                    .map_err(|e| format!("分类结果无法序列化: {e}"))
            }
            proto::methods::gateway::IS_ERROR_RESPONSE => {
                let status = params.get("status").and_then(Value::as_u64).unwrap_or(0) as u16;
                let headers: std::collections::BTreeMap<String, String> = params
                    .get("headers")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();
                Ok(Value::Bool(provider.is_error_response(status, &headers)))
            }
            proto::methods::gateway::REFUSAL_MARKERS => {
                let (finishes, markers) = provider.refusal_markers();
                Ok(serde_json::json!({ "finishes": finishes, "markers": markers }))
            }
            crate::retry::RETRY_DECISION_METHOD => {
                let status = params.get("status").and_then(Value::as_u64).unwrap_or(0) as u16;
                let body = params.get("body").cloned().unwrap_or(Value::Null);
                let attempt = params.get("attempt").and_then(Value::as_u64).unwrap_or(0) as usize;
                let budget = params.get("budget").and_then(Value::as_u64).unwrap_or(0) as usize;
                serde_json::to_value(provider.retry_decision(status, &body, attempt, budget))
                    .map_err(|error| format!("重试决定无法序列化: {error}"))
            }
            proto::methods::gateway::RETRY_ADVICE => {
                let body = params.get("body").cloned().unwrap_or(Value::Null);
                let attempt = params.get("attempt").and_then(Value::as_u64).unwrap_or(0) as usize;
                let budget = params.get("budget").and_then(Value::as_u64).unwrap_or(0) as usize;
                match provider.retry_advice(&body, attempt, budget) {
                    Some(advice) => serde_json::to_value(advice)
                        .map_err(|e| format!("退避建议无法序列化: {e}")),
                    None => Ok(Value::Null),
                }
            }
            proto::methods::gateway::REASONING_PATCH => {
                let level = params.get("level").and_then(Value::as_str).unwrap_or("");
                let model = params.get("model").and_then(Value::as_str).unwrap_or("");
                let body = params.get("body").cloned().unwrap_or(Value::Null);
                serde_json::to_value(provider.reasoning_patch(level, model, &body))
                    .map_err(|e| format!("档位结果无法序列化: {e}"))
            }
            proto::methods::gateway::COOLDOWN_MODELS => {
                let wire = params.get("wire_model").and_then(Value::as_str).unwrap_or("");
                serde_json::to_value(provider.cooldown_models(&account, wire))
                    .map_err(|e| format!("冷却结果无法序列化: {e}"))
            }
            proto::methods::gateway::TOKEN_ENSURE => {
                provider.ensure_token(&ctx, &account).await.map(Value::String)
            }
            proto::methods::gateway::TOKEN_REFRESH => {
                provider.refresh_token(&ctx, &account).await.map(Value::String)
            }
            proto::methods::gateway::TOKEN_EXPIRING => {
                provider.token_expiring(&ctx, &account).await.map(Value::Bool)
            }
            proto::methods::gateway::MODELS_LIST => provider.models(&ctx, &account).await.and_then(
                |list| serde_json::to_value(list).map_err(|e| format!("清单无法序列化: {e}")),
            ),
            proto::methods::gateway::MODELS_REFRESH => {
                let force = params.get("force").and_then(Value::as_bool).unwrap_or(false);
                provider.refresh_models_snapshot(&ctx, &account, force).await
            }
            proto::methods::gateway::USAGE_QUERY => provider.usage(&ctx, &account).await.map(|mut report| {
                let sources = provider.quota_sources(&report);
                if let Some(object) = report.as_object_mut() {
                    object.insert("quotaSources".to_string(), Value::Array(sources));
                }
                report
            }),
            proto::methods::gateway::CHECKIN_CLAIM => {
                let manual = params.get("manual").and_then(Value::as_bool).unwrap_or(false);
                crate::checkin::run(provider.as_ref(), &ctx, &account, manual).await.and_then(
                    |report| serde_json::to_value(report).map_err(|e| format!("签到结果无法序列化: {e}")),
                )
            }
            proto::methods::gateway::CLAIM => {
                let claim = params.get("claim").and_then(Value::as_str).unwrap_or("");
                provider.claim(&ctx, &account, claim).await
            }
            proto::methods::gateway::CLAIM_STATUS => {
                let refresh = params.get("refresh").and_then(Value::as_bool).unwrap_or(false);
                provider.claim_status(&ctx, &account, refresh).await
            }
            proto::methods::gateway::ADMIN_ACTION => {
                let action = params.get("action").and_then(Value::as_str).unwrap_or("");
                if action.trim().is_empty() {
                    Err("admin.action 缺少 action".to_string())
                } else {
                    let payload = params.get("payload").cloned().unwrap_or(Value::Null);
                    provider.admin_action(&ctx, &account, action, &payload).await
                }
            }
            proto::methods::gateway::ACCOUNT_NORMALIZE => {
                match serde_json::from_value::<proto::NormalizeInput>(params.clone()) {
                    Ok(input) => provider.normalize_account(&ctx, input).await.and_then(|account| {
                        serde_json::to_value(account).map_err(|e| format!("账号无法序列化: {e}"))
                    }),
                    Err(e) => Err(format!("normalize 参数无法解析: {e}")),
                }
            }
            proto::methods::gateway::ACCOUNT_VIEW => {
                let secret = params.get("secret").cloned().unwrap_or(Value::Null);
                provider.account_view(&secret)
            }
            proto::methods::gateway::ACCOUNT_IDENTITY => {
                let secret = params.get("secret").cloned().unwrap_or(Value::Null);
                provider.account_identity(&secret).map(Value::String)
            }
            proto::methods::gateway::LOGIN_START => {
                let method_name = params.get("method").and_then(Value::as_str).unwrap_or("");
                let payload = params.get("payload").cloned().unwrap_or(Value::Null);
                provider.login_start(&ctx, method_name, &payload).await.and_then(|start| {
                    serde_json::to_value(start).map_err(|e| format!("登录启动无法序列化: {e}"))
                })
            }
            proto::methods::gateway::LOGIN_POLL => {
                let flow = params.get("flow").and_then(Value::as_str).unwrap_or("");
                provider.login_poll(&ctx, flow).await.and_then(|poll| {
                    serde_json::to_value(poll).map_err(|e| format!("登录轮询无法序列化: {e}"))
                })
            }
            proto::methods::gateway::LOGIN_CALLBACK => {
                match serde_json::from_value::<proto::LoginCallback>(params.clone()) {
                    Ok(callback) => provider.login_callback(&ctx, callback).await.and_then(|poll| {
                        serde_json::to_value(poll).map_err(|e| format!("登录回调无法序列化: {e}"))
                    }),
                    Err(e) => Err(format!("callback 参数无法解析: {e}")),
                }
            }
            proto::methods::gateway::LOGIN_CANCEL => {
                let flow = params.get("flow").and_then(Value::as_str).unwrap_or("");
                provider.login_cancel(&ctx, flow).await.map(|()| Value::Null)
            }
            _ => {
                self.reply_err(
                    id,
                    proto::WireError::with_code(
                        format!("不认识的方法 {method}"),
                        proto::WireError::NO_METHOD,
                    ),
                )
                .await;
                return;
            }
        };

        match outcome {
            Ok(value) => self.reply(id, value).await,
            Err(message) => self.reply_err(id, crate::errors::to_wire_error(message)).await,
        }
    }

    /// 流式转发（唯一会发多帧的方法）。
    async fn handle_conversation(self: &Arc<Self>, id: i64, params: Value) {
        let Some(provider) = self.provider_of(&params) else {
            self.reply_err(
                id,
                proto::WireError::with_code(
                    "params.provider 缺失或未知",
                    proto::WireError::BAD_PARAMS,
                ),
            )
            .await;
            return;
        };
        let req = match serde_json::from_value::<proto::ConversationRequest>(params) {
            Ok(req) => req,
            Err(e) => {
                self.reply_err(
                    id,
                    proto::WireError::with_code(
                        format!("conversation 参数无法解析: {e}"),
                        proto::WireError::BAD_PARAMS,
                    ),
                )
                .await;
                return;
            }
        };

        let cancel = Arc::new(Cancel::default());
        let credit = Arc::new(Semaphore::new(proto::DEFAULT_WINDOW_BYTES));
        self.running
            .lock()
            .await
            .insert(id, Running { cancel: cancel.clone(), credit: credit.clone() });

        let ctx = self.ctx(&provider.provider_id());
        let mut sink = StreamSink {
            host: self.clone(),
            id,
            credit,
            cancel: cancel.clone(),
            head_sent: false,
        };
        let result = {
            let view = ConversationSink::new(&mut sink);
            // 不依赖 provider 自觉观察取消：drop Future 后清 running 与回调等待者。
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err("调用已被取消".to_string()),
                result = provider.conversation(&ctx, req, view) => result,
            }
        };

        self.running.lock().await.remove(&id);
        match result {
            _ if cancel.is_cancelled() => {
                self.reply_err(id, proto::WireError::with_code(
                    "调用已被取消", proto::WireError::ABORTED,
                )).await;
            }
            Ok(()) => self.reply(id, Value::Null).await,
            Err(message) => self.reply_err(id, crate::errors::to_wire_error(message)).await,
        }
    }

    /// 按 `params.provider` 找 provider。
    ///
    /// ── 为什么线性扫而不是建索引 ────────────────────────────────
    /// provider 数量很小（一个包最多几十家），而索引需要在 `init` 前后维护
    /// 两套状态（未 init 时索引为空、但 `chat` 可能在 init 之前到达 ——
    /// 网关不会那样做，但协议不禁止）。线性扫没有这个时序问题，代价是
    /// 每次几十次字符串比较，可以忽略。
    fn provider_of(&self, params: &Value) -> Option<Arc<dyn Provider>> {
        let id = params.get("provider").and_then(Value::as_str)?;
        self.providers.iter().find(|p| p.provider_id() == id).cloned()
    }

    fn ctx(self: &Arc<Self>, provider: &str) -> PluginCtx {
        let env = self.env.try_read().ok().and_then(|guard| guard.clone()).unwrap_or_else(|| {
            proto::HostEnv {
                data_dir: String::new(),
                host_version: String::new(),
                target: String::new(),
                proxy: String::new(),
                debug_traffic: false,
                loopback_port: 0,
            }
        });
        PluginCtx { env, call: Arc::new(CallHandle { host: Some(self.clone()), provider: provider.to_string() }) }
    }

    /// 收下一条回调应答。
    async fn settle(&self, frame: proto::Frame) {
        let waiter = self.pending.lock().await.remove(&frame.id);
        if let Some(tx) = waiter {
            let outcome = match frame.error {
                Some(err) => Err(err),
                None => Ok(frame.result.unwrap_or(Value::Null)),
            };
            let _ = tx.send(outcome);
        }
    }
}

/// 在调用方取消回调 Future 时也清理 pending，避免子进程 abort 留下等待者。
struct CallbackWaitGuard {
    host: Arc<Serve>,
    id: i64,
}

impl Drop for CallbackWaitGuard {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.host.pending.try_lock() {
            pending.remove(&self.id);
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let host = self.host.clone();
            let id = self.id;
            runtime.spawn(async move { host.pending.lock().await.remove(&id); });
        }
    }
}

/// 回调发送端：`PluginCtx` 用它把请求发给网关并等应答。
struct CallHandle {
    host: Option<Arc<Serve>>,
    provider: String,
}

impl CallGateway for CallHandle {
    fn call<'a>(
        &'a self,
        method: &'a str,
        mut params: Value,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Value, StoreError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let Some(host) = &self.host else {
                return Err(StoreError::Unavailable("插件宿主尚未启动".into()));
            };
            if method == proto::methods::plugin::STORE_UPDATE {
                let next = params.get("secret").cloned().unwrap_or(Value::Null);
                if let Some(adapter) = host.provider_of(&serde_json::json!({ "provider": self.provider })) {
                    if let Ok(public) = adapter.account_view(&next) {
                        if let Some(fields) = params.as_object_mut() {
                            fields.insert("public".to_string(), public);
                        }
                    }
                }
            }
            let id = host.next_id.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = oneshot::channel();
            host.pending.lock().await.insert(id, tx);
            let _waiter = CallbackWaitGuard { host: host.clone(), id };
            let frame = proto::Frame {
                id,
                method: Some(method.to_string()),
                params: Some(params),
                ..Default::default()
            };
            // 回调 Future 可能被取消；实际写帧独立完成，不能留下半行 JSON。
            let writing = {
                let host = host.clone();
                tokio::spawn(async move { host.write(frame).await })
            };
            match writing.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    return Err(StoreError::Unavailable(format!("写网关失败: {error}")));
                }
                Err(_) => return Err(StoreError::Unavailable("写网关任务退出".into())),
            }
            match rx.await {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(err)) => {
                    if err.code.as_deref() == Some("not_found") {
                        Err(StoreError::NotFound)
                    } else {
                        Err(StoreError::Rejected(err.message))
                    }
                }
                Err(_) => Err(StoreError::Unavailable("网关没有应答".into())),
            }
        })
    }
}

/// 流式输出的实现。
struct StreamSink {
    host: Arc<Serve>,
    id: i64,
    credit: Arc<Semaphore>,
    cancel: Arc<Cancel>,
    head_sent: bool,
}

impl SinkInner for StreamSink {
    fn cancellation(&self) -> Arc<Cancel> {
        self.cancel.clone()
    }

    fn send_head<'a>(&'a mut self, head: proto::StreamHead) -> crate::provider::SendFuture<'a, ()> {
        Box::pin(async move {
            if self.cancel.is_cancelled() {
                return Err("调用已被取消".to_string());
            }
            if self.head_sent {
                return Err("head 只能发一次".to_string());
            }
            self.head_sent = true;
            let mut headers = std::collections::BTreeMap::new();
            for (k, v) in head.headers {
                headers.insert(k.to_ascii_lowercase(), v);
            }
            let upstream = match serde_json::to_value(head.upstream) {
                Ok(Value::String(name)) => Some(name),
                _ => None,
            };
            let frame = proto::Frame {
                id: self.id,
                event: Some("head".into()),
                status: Some(head.status),
                headers: Some(headers),
                upstream,
                ..Default::default()
            };
            // 写帧不能中途取消，否则半行 JSON 会破坏其它请求的管道边界。
            self.host.write_stream_frame(frame, self.cancel.clone()).await.map_err(|e| format!("写 head 失败: {e}"))
        })
    }

    fn send_chunk<'a>(&'a mut self, bytes: &'a [u8]) -> crate::provider::SendFuture<'a, ()> {
        Box::pin(async move {
            if !self.head_sent {
                return Err("必须先发 head 再发 chunk".to_string());
            }
            // ── 为什么要切片 ─────────────────────────────────────
            // 额度窗口按「载荷 + 帧开销」计费，而一次 `send` 的载荷可能远大于
            // 窗口（作者直接丢进来一整段）。不切的话 `acquire_many` 会永远
            // 等不到那么多额度 —— 表现为流卡死，而不是报错。
            // 切成 MAX_CHUNK_BYTES 之后，单帧成本恒小于初始窗口。
            for piece in bytes.chunks(proto::MAX_CHUNK_BYTES) {
                if self.cancel.is_cancelled() {
                    return Err("调用已被取消".to_string());
                }
                let cost = piece.len() + proto::FRAME_OVERHEAD_BYTES;
                let permit = tokio::select! {
                    biased;
                    _ = self.cancel.cancelled() => return Err("调用已被取消".to_string()),
                    acquired = self.credit.clone().acquire_many_owned(cost as u32) => {
                        acquired.map_err(|_| "调用已被取消".to_string())?
                    }
                };
                // ── 为什么是 `forget()` 而不是 drop ─────────────────
                // drop 会把额度**立刻还回信号量**，于是窗口永远是满的 ——
                // 背压完全失效（实测：不还 credit 时插件照样把 4 MiB 全推
                // 完）。协议语义是「已发未还」：额度只有网关发 `credit{n}`
                // 时才该补回，那件事发生在 `dispatch` 里（`add_permits`）。
                // `forget()` 让这份 permit 永久消耗，正是要的语义。
                permit.forget();
                let frame = proto::Frame {
                    id: self.id,
                    event: Some("chunk".into()),
                    data: Some(proto::encode_chunk(piece)),
                    ..Default::default()
                };
                self.host.write_stream_frame(frame, self.cancel.clone()).await.map_err(|e| format!("写 chunk 失败: {e}"))?;
            }
            Ok(())
        })
    }
}
