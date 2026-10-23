//! 示例 provider：验证整条链路，同时是写新 provider 的模板。
//!
//! ── 这个实现是干什么的 ──────────────────────────────────────
//! 它不是真的接某家上游，而是把插件协议里每一条能力都走一遍：
//!   - 无状态转发（`build_chat`）：构造一个指向本地假上游的请求；
//!   - 有状态转发（`conversation`）：自己发请求、自己推 chunk；
//!   - 错误分类、退避建议、思考档位、模型目录、余额、签到；
//!   - 账号规范化与登录流程（含回调型与设备授权型两种）；
//!   - 账号读写回调（`ctx.store_get` / `ctx.store_update`）。
//!
//! 端到端验证脚本（`scripts/verify-plugin-host.mjs`）用它跑通：
//! init → chat → 流式 → 背压 → abort → 回调 → 崩溃恢复。
//!
//! ── 写新 provider 时怎么用这个文件 ───────────────────────────
//! 抄结构，不要抄逻辑。要点：
//!   1. `meta()` 申报能力位 —— **默认值不是结论**，能做的才填 true；
//!   2. 只实现你真正支持的方法，其余保持默认（默认会返回「不支持」）；
//!   3. 所有出网都走 `reqwest::Client`，代理用 `req.proxy`；
//!   4. 错误返回 `Err(String)`，不要 panic（crate 已 deny unwrap/expect）。

use std::collections::BTreeMap;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{
    ChatPlan, ConversationSink, Provider, ProviderMeta, SendFuture,
};
use serde_json::{json, Value};

/// 示例 provider 的 id（`pack.json` 里也要写同一个）。
pub const ID: &str = "demo";

pub struct DemoProvider;

impl Provider for DemoProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(ID, "示例提供商")
            .with_capabilities(proto::Capabilities {
                chat: true,
                // 这个示例同时演示会话式路径，所以 stateful 也是 true。
                // 真实的 provider 二者只能有一个为 true。
                stateful: true,
                refresh: true,
                model_refresh: true,
                usage: true,
                checkin: true,
                claims: false,
                web_login: true,
                sse_model_rewrite: true,
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
                    label: "示例令牌".into(),
                    placeholder: "粘贴 demo_ 开头的令牌".into(),
                    extra_fields: vec![],
                },
                proto::LoginMethod::Browser {
                    button: "在浏览器中登录".into(),
                    hint: Some("示例：设备授权型流程".into()),
                },
            ])
            .with_models(vec![
                json!({ "id": "demo-fast", "name": "Demo Fast", "context_length": 8192 }),
                json!({ "id": "demo-slow", "name": "Demo Slow", "context_length": 32768 }),
            ])
            // 账号行的渲染参数：这里给的是**最小一份**，照抄后按自己家改。
            // 每一项的语义见协议里 `AccountFeatures` 的文档。
            .with_account_features(proto::AccountFeatures {
                // 有余额 / 积分查询概念 → 账号行给「余额」入口
                usage: true,
                // 余额明细里有逐份积分包 → 读数旁挂「积分明细」（没实现就别开）
                credit_detail: false,
                // 账号标识读 `account_view` 里的哪个键（按顺序取第一个有值的）
                identifier: vec!["uid".to_string()],
                // 有效期读哪个键；空 = 这一列没有可读的字段
                expiry: vec!["expiresAt".to_string()],
                ..Default::default()
            })
    }

    // ── 无状态转发 ────────────────────────────────────────────

    fn build_chat<'a>(
        &'a self,
        _ctx: &'a agent2api_provider_sdk::PluginCtx,
        req: proto::ChatRequest,
    ) -> SendFuture<'a, ChatPlan> {
        Box::pin(async move {
            // ── 真实 provider 在这一步做三件事 ──────────────────
            // ① 取凭证（`ctx.store_get` 拿 secret，必要时刷新）；
            // ② 拼 URL（各家路径不同，多数是 `{base}/chat/completions`）；
            // ③ 拼头（鉴权头 + 一批客户端指纹头）与改写 body。
            //
            // 示例：把请求指到回环地址上的假上游（端口从环境变量取，
            // 验证脚本会设它；也可以用 `x-demo-upstream` 头逐请求指定）。
            let base = req
                .headers
                .get("x-demo-upstream")
                .cloned()
                .filter(|value| !value.is_empty())
                .or_else(|| std::env::var("DEMO_UPSTREAM").ok())
                .unwrap_or_else(|| "http://127.0.0.1:18080".to_string());
            let mut headers = vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("authorization".to_string(), format!("Bearer demo-{}", req.account)),
            ];
            // 把客户端的自定义头透传一部分（真实 provider 通常只取需要的几个）
            for (key, value) in &req.headers {
                if key.starts_with("x-demo-") {
                    headers.push((key.clone(), value.clone()));
                }
            }
            // body 原样透传，只把 model 换成网关解析出的发送名
            let mut body = req.body.clone();
            if let Some(object) = body.as_object_mut() {
                object.insert("model".into(), Value::String(req.wire_model.clone()));
            }
            Ok(ChatPlan::chat(format!("{base}/v1/chat/completions"), headers, body))
        })
    }

    // ── 有状态转发 ────────────────────────────────────────────

    fn conversation<'a>(
        &'a self,
        ctx: &'a agent2api_provider_sdk::PluginCtx,
        req: proto::ConversationRequest,
        mut sink: ConversationSink<'a>,
    ) -> SendFuture<'a, ()> {
        Box::pin(async move {
            // ── 演示三件事：回调、流式推帧、取消 ────────────────
            // ① 回调网关读账号（真实 provider 用它取凭证）
            let account = ctx.store_get(ID, &req.account).await;
            let account_hint = match &account {
                Ok(found) => found.id.clone(),
                Err(_) => req.account.clone(),
            };

            // ② 发一个真实的上游请求（示例指向假上游）
            //
            // ── 地址的三个来源（优先级从高到低）──────────────────
            //   1. `x-demo-upstream` 请求头：测试与验证脚本用它指定端口
            //      （插件的环境变量在 spawn 时固定，进程起来之后改不了 ——
            //      而一个测试进程里会有多个假上游）；
            //   2. `DEMO_UPSTREAM` 环境变量：部署时的静态配置；
            //   3. 回环 18080：没有配置时的兜底。
            let base = req
                .headers
                .get("x-demo-upstream")
                .cloned()
                .filter(|value| !value.is_empty())
                .or_else(|| std::env::var("DEMO_UPSTREAM").ok())
                .unwrap_or_else(|| "http://127.0.0.1:18080".to_string());
            let client = reqwest::Client::new();
            let response = client
                .post(format!("{base}/v1/chat/completions"))
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer demo-{account_hint}"))
                .json(&req.body)
                .send()
                .await
                .map_err(|e| format!("上游请求失败: {e}"))?;

            let status = response.status().as_u16();
            let mut headers = BTreeMap::new();
            for (key, value) in response.headers() {
                if let Ok(text) = value.to_str() {
                    headers.insert(key.as_str().to_string(), text.to_string());
                }
            }
            // ③ 先报头（网关据此决定状态码与翻译机）
            sink.head(proto::StreamHead {
                status,
                headers,
                upstream: proto::UpstreamKind::Chat,
            })
            .await?;

            // ④ 逐段推正文。`send` 在背压窗口用完时会挂起 —— 不要自己
            //    缓冲整段回答再推，那会让背压失效。
            let mut stream = response.bytes_stream();
            use futures::StreamExt;
            while let Some(chunk) = stream.next().await {
                let bytes = chunk.map_err(|e| format!("读上游失败: {e}"))?;
                sink.send(&bytes).await?;
            }
            Ok(())
        })
    }

    // ── 错误与策略 ────────────────────────────────────────────

    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        // ── 示例只演示四档的判定形状 ────────────────────────
        // 真实 provider 的判据各不同（见计划里「错误分类」一节）：
        // 有些上游把限额与速率窗口都用 429 表达，只能靠文案区分。
        let message = body
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("上游返回错误")
            .to_string();
        let upstream_code = body
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(Value::as_i64);
        match status {
            429 => proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code,
                status,
            },
            401 | 403 => proto::ErrorClass::TokenExpired { message },
            // 400 + 审核文案 = 内容拦截（不罚账号）
            400 if message.contains("sensitive") => proto::ErrorClass::ContentBlocked {
                status,
                message,
                upstream_code,
            },
            _ => proto::ErrorClass::Fatal { status, message, upstream_code },
        }
    }

    fn retry_advice(&self, body: &Value, attempt: usize, budget: usize) -> Option<proto::RetryAdvice> {
        // 示例：正文含 `retry_later` 时退避重试
        let text = body.to_string();
        if !text.contains("retry_later") || attempt + 1 >= budget {
            return None;
        }
        Some(proto::RetryAdvice {
            delay_ms: 500 * (attempt as u64 + 1),
            reason: "示例：上游要求稍后重试".to_string(),
        })
    }

    fn reasoning_patch(&self, level: &str, _model: &str, _body: &Value) -> proto::ReasoningPatch {
        // 示例只认三个档位；其它一律不注入（并把原因说清楚）
        match level {
            "low" | "medium" | "high" => proto::ReasoningPatch::Set {
                field: "reasoning_effort".to_string(),
                value: Value::String(level.to_string()),
            },
            "" => proto::ReasoningPatch::Skip { reason: "请求没有绑定思考档位".to_string() },
            other => proto::ReasoningPatch::Skip {
                reason: format!("示例 provider 只认 low/medium/high，收到 {other}"),
            },
        }
    }

    fn refusal_markers(&self) -> (Vec<String>, Vec<String>) {
        (
            vec!["content_policy_violation".to_string()],
            vec!["insufficient_quota".to_string()],
        )
    }

    // ── 凭证 ──────────────────────────────────────────────────

    fn ensure_token<'a>(
        &'a self,
        ctx: &'a agent2api_provider_sdk::PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, String> {
        Box::pin(async move {
            // ── 演示「读账号 + 条件回写」的完整形态 ──────────────
            let found = ctx.store_get(ID, account).await.map_err(String::from)?;
            let token = found
                .secret
                .get("token")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if token.is_empty() {
                return Err("账号里没有令牌，请重新登录".to_string());
            }
            // 临期时刷新（示例：把过期时间往后推 1 小时并条件回写）
            let expires_at = found.secret.get("expires_at").and_then(Value::as_i64).unwrap_or(0);
            let now = now_ms();
            if expires_at > 0 && expires_at - now < 5 * 60 * 1000 {
                let mut secret = found.secret.clone();
                if let Some(object) = secret.as_object_mut() {
                    object.insert("expires_at".into(), json!(now + 3600 * 1000));
                }
                // 条件回写：只有值还是我们读到的那份时才写
                ctx.store_update(&found.id, &found.secret, &secret)
                    .await
                    .map_err(String::from)?;
            }
            Ok(token)
        })
    }

    fn token_expiring<'a>(
        &'a self,
        ctx: &'a agent2api_provider_sdk::PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, bool> {
        Box::pin(async move {
            let Ok(found) = ctx.store_get(ID, account).await else {
                // 判不出来就 false：宁可漏刷，也不要凭猜测打上游
                return Ok(false);
            };
            let expires_at = found.secret.get("expires_at").and_then(Value::as_i64).unwrap_or(0);
            Ok(expires_at > 0 && expires_at - now_ms() < 5 * 60 * 1000)
        })
    }

    // ── 目录 ──────────────────────────────────────────────────

    fn models<'a>(
        &'a self,
        _ctx: &'a agent2api_provider_sdk::PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async move {
            // 示例：第一次问就返回内置清单，并假装「已从远程刷新过」
            Ok(proto::ModelList {
                models: self.meta().default_models,
                meta: proto::CatalogMeta { remote: false, refreshed_at: 0 },
            })
        })
    }

    fn refresh_models<'a>(
        &'a self,
        _ctx: &'a agent2api_provider_sdk::PluginCtx,
        _account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            if !force {
                // 自动路径：按缓存跳过（正常行为，不是失败）
                return Ok(proto::RefreshOutcome::unchanged());
            }
            // 手动路径：真去拉一次。示例里直接返回内置清单。
            let models = self.meta().default_models;
            Ok(proto::RefreshOutcome::refreshed(models.len()))
        })
    }

    // ── 余额 / 签到 ───────────────────────────────────────────

    fn usage<'a>(
        &'a self,
        _ctx: &'a agent2api_provider_sdk::PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::UsageReport> {
        Box::pin(async move {
            Ok(json!({
                "plan": "Demo Plan",
                "balance": "1000",
                "user": account,
                "windows": [
                    { "name": "每日", "used": 12.5, "limit": 100, "unit": "credits" }
                ],
            }))
        })
    }

    fn checkin<'a>(
        &'a self,
        _ctx: &'a agent2api_provider_sdk::PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(async move {
            Ok(proto::CheckinReport {
                outcome: proto::CheckinOutcome::Claimed,
                credit: Some("+10 credits".to_string()),
                streak: Some(3),
                message: None,
            })
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    fn normalize_account<'a>(
        &'a self,
        _ctx: &'a agent2api_provider_sdk::PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move {
            // ── 四种来源都要处理（真实 provider 的处理各不相同）──
            let (token, name) = match &input.source {
                proto::NormalizeSource::Manual { payload } => {
                    let token = payload
                        .get("token")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = payload
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("示例账号")
                        .to_string();
                    (token, name)
                }
                proto::NormalizeSource::FileText { content, .. } => {
                    let parsed: Value =
                        serde_json::from_str(content).map_err(|e| format!("文件不是合法 JSON: {e}"))?;
                    let token = parsed
                        .get("token")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    (token, "示例账号（文件导入）".to_string())
                }
                proto::NormalizeSource::FileBase64 { .. } => {
                    return Err("示例 provider 不支持二进制文件导入".to_string());
                }
                proto::NormalizeSource::Desktop => {
                    // 「实时读本机客户端」型：不落凭证，只记一个标记
                    (String::new(), "示例账号（桌面端）".to_string())
                }
            };
            if token.is_empty() && !matches!(input.source, proto::NormalizeSource::Desktop) {
                return Err("令牌不能为空".to_string());
            }
            let identity = if token.is_empty() {
                "demo-desktop".to_string()
            } else {
                // 真实 provider 用上游给的稳定 uid；示例用令牌的哈希前缀
                format!("demo-{:x}", simple_hash(&token))
            };
            Ok(proto::NormalizedAccount {
                secret: json!({
                    "token": token,
                    "expires_at": now_ms() + 24 * 3600 * 1000,
                    "desktop": token.is_empty(),
                }),
                public: json!({
                    "name": input.name.clone().unwrap_or(name),
                    "uid": identity,
                    "tokenTail": tail_of(&token),
                    "expiresAt": now_ms() + 24 * 3600 * 1000,
                }),
                identity,
                desktop: token.is_empty(),
            })
        })
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        let token = secret.get("token").and_then(Value::as_str).unwrap_or("");
        if token.is_empty() {
            Ok("demo-desktop".to_string())
        } else {
            Ok(format!("demo-{:x}", simple_hash(token)))
        }
    }

    // ── 登录 ──────────────────────────────────────────────────

    fn login_start<'a>(
        &'a self,
        _ctx: &'a agent2api_provider_sdk::PluginCtx,
        method: &'a str,
        _payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(async move {
            match method {
                // 设备授权型：网关打开 url，然后按间隔轮询
                "browser" => Ok(proto::LoginStart {
                    flow: "demo-flow".to_string(),
                    url: "https://example.com/demo-login".to_string(),
                    instructions: "在打开的页面里完成登录".to_string(),
                    poll_interval_ms: 3000,
                    external_browser: false,
                    allowed_hosts: vec!["example.com".to_string()],
                }),
                // 回调型：网关打开 url，等用户完成后调 callback
                "callback" => Ok(proto::LoginStart {
                    flow: "demo-callback".to_string(),
                    url: "https://example.com/demo-oauth".to_string(),
                    instructions: String::new(),
                    poll_interval_ms: 0,
                    external_browser: true,
                    allowed_hosts: vec!["example.com".to_string(), "127.0.0.1".to_string()],
                }),
                other => Err(format!("示例 provider 不支持登录方式 {other}")),
            }
        })
    }

    fn login_poll<'a>(
        &'a self,
        _ctx: &'a agent2api_provider_sdk::PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async move {
            // 示例：第一次轮询就成功（真实 provider 要真去问上游）
            if flow != "demo-flow" {
                return Err(format!("没有这个登录流程: {flow}"));
            }
            Ok(proto::LoginPoll::Done {
                account: proto::NormalizedAccount {
                    secret: json!({ "token": "demo-device-token", "expires_at": now_ms() + 86400_000 }),
                    public: json!({ "name": "示例账号（设备登录）", "uid": "demo-device" }),
                    identity: "demo-device".to_string(),
                    desktop: false,
                },
            })
        })
    }
}

/// 当前毫秒时间戳。
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 一个稳定的简单哈希（**不是**密码学哈希，示例专用）。
fn simple_hash(input: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 令牌尾部四位（界面显示用）。
fn tail_of(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let start = chars.len().saturating_sub(4);
    chars[start..].iter().collect()
}
