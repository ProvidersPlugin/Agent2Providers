//! Jev AI（jev-ai.pro）—— TypeSafe Jev 判决模型的第三方托管站。
//!
//! ── 这家为什么没有「对话」────────────────────────────────────
//! 上游只提供**判决**接口：`POST /api/v1/systemone`（body 是 `state` + `questions`，
//! 返回 noul / choice / score 的概率），没有任何 chat/completions 兼容层。
//! 排除过程（2026-11-16 实测，全部打的是 `https://jev-ai.pro`）：
//!
//! ```text
//!   GET  /api/v1/models            → 401（有 key 时 200，返回 15 个判决模型名）
//!   GET  /api/v1/credits           → 401（有 key 时 200，creditsRemaining / paidInputTokensRemaining）
//!   POST /api/v1/systemone         → 200（判决接口，不是对话）
//!   POST /api/v1/web-context       → 存在（带联网证据的判决）
//!   POST /api/alpha/decisions      → 存在（Decisions 兼容壳，同样是判决）
//!   GET  /api/v1/me, /api/v1/user, /api/v1/account  → 404
//! ```
//!
//! 官方文档（`/docs`、`/jev-api`）的 endpoint 清单也只有上面几条，
//! 且明说「Jev 不生成文本，只打分」。所以 `capabilities.chat = false` 是
//! **上游确实没有**，不是「还没做」——按 README 六列口径，这一列打 ✗。
//!
//! ── 它有什么：签到与余额 ─────────────────────────────────────
//! `GET /api/daily-checkin`（带会话 Cookie）**调用即签到**，同一次响应
//! 还带余额，所以这一家不需要 API key：
//!
//! ```json
//! {"timeZone":"UTC","enabled":true,"rewards":[1,1,2,1,1,2,3],"expiryDays":30,
//!  "claimedToday":true,"currentCycleDay":1,"completedCycleDay":1,"streak":1,
//!  "lastClaimedAt":"2026-11-16T01:30:08.906Z",
//!  "nextAvailableAt":"2026-11-17T00:00:00.000Z",
//!  "activeCredits":7,"nextExpiryAt":"2026-12-11T01:47:05.303Z","balance":27}
//! ```
//!
//! POST 同一路径 → 401（带 Cookie 也是），`/daily-checkin/claim`、`/status` 均 404
//! → 没有独立的领取路由，签到就是那一次 GET。
//!
//! ── 鉴权：只认会话 Cookie，而且是 rolling 的 ────────────────
//! `/api/daily-checkin` 只认 `jev-ai-session=Fe26.2**…`（iron-session 密封），
//! `Authorization: Bearer sk_jev-ai_…` 对它无效（实测 401 Unauthorized）。
//! 反过来 `/api/v1/*` 只认 API key。两套互不通用，本实现只走 Cookie 那套。
//!
//! 成功的响应会通过 `Set-Cookie` **换发一个新密封体**，旧的随即作废
//! （实测：一次成功调用之后，同一把 Cookie 再打就是 401）。所以每次调用
//! 都要把新的那份条件写回账号（见 `persist_rotation`），否则第二天的自动
//! 签到必然认不出凭证。401/403 响应也会下发新密封体 —— 那是给未登录访客的
//! 空会话，**不能**写回，否则等于自己把凭证换成一把废的。
//!
//! ── UTC 翻页 vs 网关的北京日（这条最要命）───────────────────
//! 上游 `timeZone: "UTC"`，`nextAvailableAt` 落在 00:00Z —— **jev 的一天在
//! 北京时间 08:00 才翻**。而网关的签到日按北京时间划（00:01 主轮 + 补签轮 +
//! 窗口重试），于是 00:01 那次调用看到的是**昨天 UTC 日**的已签状态。
//!
//! 若这时返回 `Done`，网关会把「今天」记成已领，当天真正的窗口（08:00 之后）
//! 就再没有人去签 —— 表现为**隔天才签一次**。所以这里这样分档：
//!
//! | 观察到的状态 | 返回 |
//! |---|---|
//! | `claimedToday` 且 `lastClaimedAt` 在 3 分钟内 | `Claimed`（刚签成） |
//! | `claimedToday` 且 `lastClaimedAt` 是**今天（北京）** | `Done`（幂等，不重试） |
//! | `claimedToday` 但 `lastClaimedAt` 是**昨天（北京）** | `Inactive` + `retry_at = nextAvailableAt` |
//! | `!claimedToday` 且未到 `nextAvailableAt` | `Inactive` + `retry_at` |
//! | `!claimedToday` 且已过窗口却仍没签上 | `Failed`（真失败，按网关策略重试） |
//!
//! `retry_at` 由网关的窗口重试轮消费（`auto_checkin::retry` 只认
//! `claim.retryAt`，并保证不早于 `now + 30s`），所以 08:00 那一轮会自己到点。
//!
//! ── 身份与续期 ─────────────────────────────────────────────
//! Cookie 没有续期接口（`refresh = false`），失效只能重新登录再粘贴。
//! 上游也没有可读的稳定用户 id（`/api/me`、`/api/account` 都取不到），
//! 所以 `identity` 用 Cookie 的哈希：**重新登录会变成一条新账号**，
//! 这是已知取舍，见 `normalize_account` 的注释。

use std::time::Duration;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{CheckinResult, PluginCtx, Provider, ProviderMeta, SendFuture};
use chrono::{DateTime, FixedOffset, Utc};
use serde_json::{json, Value};

/// provider id（`pack.json` 里必须写同一个）。
pub const ID: &str = "jev-ai";

const BASE: &str = "https://jev-ai.pro";
/// 签到 + 余额都在这一条 GET 上（调用即签到）。
const CHECKIN_PATH: &str = "/api/daily-checkin";
/// 浏览器化 UA：上游在 Cloudflare 后面，缺这些头会被拦在业务层之前。
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/157.0.0.0 Safari/537.36";
/// 「刚签成」的判定窗口：`lastClaimedAt` 落在这段时间内就认为这次调用签到了。
const JUST_CLAIMED_MS: i64 = 3 * 60 * 1000;

pub struct JevAiProvider;

impl Provider for JevAiProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(ID, "Jev AI（判决模型 · 签到）")
            .with_capabilities(proto::Capabilities {
                // 上游没有 chat/completions，只有系统一判决接口 —— 见模块头。
                chat: false,
                stateful: false,
                // Cookie 没有续期接口，失效只能重登。
                refresh: false,
                // 判决模型名不能当对话模型用，所以不广告目录：报一个用不了的
                // 清单比不报更糟（客户端会选中它然后稳定 400）。
                model_refresh: false,
                usage: true,
                checkin: true,
                // 20 个欢迎额度是注册即送，没有领取入口。
                claims: false,
                web_login: false,
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                refresh_uses_account: true,
                supports_default_model: false,
                env_credentials: false,
            })
            .with_login(vec![proto::LoginMethod::Token {
                label: "会话 Cookie".into(),
                placeholder: "粘贴 jev-ai-session=Fe26.2**…（或整条 cURL）".into(),
                extra_fields: vec![],
            }])
            .with_account_features(proto::AccountFeatures {
                usage: true,
                // 余额只有「可用额度 / 活跃额度」两个数，没有逐份积分包。
                credit_detail: false,
                credit_expiry: true,
                usage_detail: false,
                identifier: vec!["uid".to_string()],
                // 有效期读不到（Cookie 自带的过期时间在密封体里，不解析）。
                expiry: vec![],
                ..Default::default()
            })
            .with_checkin(proto::CheckinInfo {
                description: Some(
                    "jev-ai.pro 每日签到：GET /api/daily-checkin 调用即触发；\
                     上游按 UTC 日结算，即北京时间 08:00 翻页，窗口未到时自动等到点重试。"
                        .into(),
                ),
                daily: true,
                // 欢迎额度是注册即送，没有领取动作。
                onboarding: false,
                plans: false,
                growth: false,
                exclude_guest: false,
                modes: vec![],
                ui_hooks: vec![],
            })
    }

    // ── 登录方式元数据 ────────────────────────────────────────

    /// `auth.methods`：网关 3.5 起的动态登录方式清单。
    ///
    /// ── 为什么只有一个「填写凭证」而没有「网页登录」──────────────
    /// 这一家的登录态只落在 `jev-ai.pro` 域的 `jev-ai-session` Cookie 里，
    /// 上游没有设备授权 / 一次性 code 之类能由插件自己轮询收下的接口
    /// （对比千问办公的 PKCE 设备授权，`qoder/oauth.rs`：那条路插件自己
    /// 就能拿到令牌）。而「在内嵌窗口登录后由壳侧读 Cookie 交回」目前在
    /// 网关里是 **KukuAI 专属硬编码**（`desktop-tauri/src/login.rs` 的
    /// `run_embedded` + `read_window_cookies` + `kuku_cookie_header`），
    /// 插件侧没有任何办法让壳去读任意域的 Cookie。所以这里不挂一个
    /// 「点了也完不成」的网页登录按钮，只给表单。
    ///
    /// 想升级成真正的内嵌登录，需要网关把上面那套泛化成「按插件申报的域
    /// 读 Cookie」，那是网关侧改动 + 重新编译桌面应用，不是插件能单方面做到的。
    fn admin_action<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
        action: &'a str,
        _payload: &'a Value,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            match action {
                "auth.methods" => Ok(login_methods()),
                other => Err(agent2api_provider_sdk::unsupported(format!(
                    "Jev AI 没有这个管理动作：{other}"
                ))),
            }
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    fn normalize_account<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move {
            let (raw, name) = match &input.source {
                proto::NormalizeSource::Manual { payload } => {
                    // ── 表单字段可能在哪一层 ──────────────────────────
                    // 网关按 `auth.methods` 的 `prompts` 收集输入后原样转发，
                    // 但不同版本/不同入口的包装不一样：命中过的是
                    // `{methodIndex, inputs:{cookie}}`（字段在 `inputs` 里），
                    // 另有把字段平铺在顶层的形态。两种都认，别让「Cookie 不能为空」
                    // 这种误导性报错再出现一次。
                    let raw = pick_str(payload, &["cookie", "token", "session"]).unwrap_or_default();
                    let name = input
                        .name
                        .clone()
                        .or_else(|| pick_str(payload, &["name"]));
                    (raw, name)
                }
                proto::NormalizeSource::FileText { content, .. } => {
                    // 允许整条 cURL：从里面抠 `-b "…"` / `Cookie: …` 那段。
                    (content.clone(), input.name.clone())
                }
                proto::NormalizeSource::FileBase64 { .. } => {
                    return Err("Jev AI 的凭证是会话 Cookie 文本，不支持二进制文件导入".to_string())
                }
                proto::NormalizeSource::Desktop => {
                    return Err(
                        "Jev AI 没有本机客户端登录态可读，请从浏览器复制 jev-ai-session 后粘贴"
                            .to_string(),
                    )
                }
            };

            let session = extract_session(&raw)?;
            let uid = identity_of(&session);
            let label = name.unwrap_or_else(|| "Jev AI 账号".to_string());
            // 可选：API key 只用于余额（`/api/v1/credits`）。签到那条路只认 Cookie。
            let api_key = match &input.source {
                proto::NormalizeSource::Manual { payload } => {
                    pick_str(payload, &["apiKey", "api_key"])
                }
                _ => None,
            };
            let mut secret = json!({
                "session": session.clone(),
                "cookie": format!("jev-ai-session={session}"),
                "name": label,
                "added_at": now_ms(),
            });
            if let (Some(key), Some(object)) = (api_key, secret.as_object_mut()) {
                object.insert("apiKey".into(), Value::String(key));
            }
            // 公开形态只留尾号：`jev-ai-session` 是登录态，等于密码。
            Ok(proto::NormalizedAccount {
                secret,
                public: json!({
                    "name": label,
                    "uid": uid,
                    "tokenTail": tail_of(&session, 6),
                    // 读不到真实有效期；留 0 让界面显示「长效」而不是编一个时间。
                    "expiresAt": 0,
                    "longLived": true,
                }),
                // ── 为什么 identity 是 Cookie 的哈希而不是上游 uid ──
                // 上游没有可读的稳定用户 id（`/api/me`、`/api/account` 取不到，
                // 签到响应里也只有余额与连签天数）。因此重新登录换 Cookie 后
                // 会被当成新账号 —— 这是已知取舍，界面上表现为多一条记录。
                // 换到上游给出稳定 id 的那天，把这里改成读那个 id 即可。
                identity: uid,
                desktop: false,
            })
        })
    }

    fn account_view(&self, secret: &Value) -> Result<proto::AccountView, String> {
        let session = secret
            .get("session")
            .and_then(Value::as_str)
            .ok_or_else(|| "账号里没有会话 Cookie".to_string())?;
        let name = secret
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Jev AI 账号");
        Ok(json!({
            "name": name,
            "uid": identity_of(session),
            "tokenTail": tail_of(session, 6),
            "expiresAt": 0,
            "longLived": true,
        }))
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        let session = secret
            .get("session")
            .and_then(Value::as_str)
            .ok_or_else(|| "账号里没有会话 Cookie".to_string())?;
        Ok(identity_of(session))
    }

    // ── 签到 ──────────────────────────────────────────────────

    fn checkin_with_options<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        _manual: bool,
    ) -> SendFuture<'a, CheckinResult> {
        Box::pin(async move {
            let found = ctx.store_get(ID, account).await.map_err(String::from)?;
            let cookie = cookie_of(&found.secret)?;
            let (state, rotated) = fetch_state(&cookie).await?;
            persist_rotation(ctx, &found, rotated).await;
            Ok(report_of(&state))
        })
    }

    // ── 余额 ──────────────────────────────────────────────────

    fn usage<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::UsageReport> {
        Box::pin(async move {
            let found = ctx.store_get(ID, account).await.map_err(String::from)?;
            let cookie = cookie_of(&found.secret)?;
            match fetch_state(&cookie).await {
                Ok((state, rotated)) => {
                    persist_rotation(ctx, &found, rotated).await;
                    Ok(report_from_checkin(&state))
                }
                Err(error) => {
                    // ── 会话失效时的退路 ───────────────────────────────
                    // `/api/v1/credits` 走 API key，不依赖浏览器会话。账号里
                    // 存了 key 就还能读余额（签到仍然只能靠 Cookie）。
                    let key = found
                        .secret
                        .get("apiKey")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty());
                    let Some(key) = key else { return Err(error) };
                    match fetch_credits(key).await {
                        Ok(report) => Ok(report),
                        Err(key_error) => Err(format!(
                            "签到会话不可用（{error}）；改用 API key 读余额也失败：{key_error}"
                        )),
                    }
                }
            }
        })
    }
}

/// 把 `/api/daily-checkin` 的响应转成余额报告。
fn report_from_checkin(state: &Value) -> Value {
    let balance = state.get("balance").and_then(Value::as_i64);
    let active = state.get("activeCredits").and_then(Value::as_i64);
    let streak = state.get("streak").and_then(Value::as_i64);
    let cycle = state.get("currentCycleDay").and_then(Value::as_i64);
    let next_available = state
        .get("nextAvailableAt")
        .and_then(Value::as_str)
        .and_then(parse_ms);
    let next_expiry = state
        .get("nextExpiryAt")
        .and_then(Value::as_str)
        .and_then(parse_ms);

    let claimed_today = state
        .get("claimedToday")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let window_note = match next_available {
        Some(at) => format!("下次可签 {}", beijing_text(at)),
        None => "下次可签时间未返回".to_string(),
    };

    let mut report = json!({
        // ── 余额列读的是 `available` / `totalLeft` ──────────────────
        // 网关的 `usage_records::extract_remaining` 也只认这两个键（先
        // `totalLeft`，再 `available`），`balance` 只是文案键。
        "available": balance,
        "availableView": balance.map(|value| format!("{value} 额度")),
        "unit": "额度",
        // 界面按这几个已知键渲染；读不到的留空，不写 0。
        "plan": match (cycle, streak) {
            (Some(cycle), Some(streak)) => format!("周期第 {cycle} 天 · 连签 {streak} 天"),
            _ => "Jev AI 额度".to_string(),
        },
        "balance": balance,
        "credits": active,
        "until": next_expiry,
        // 恒为空数组：见上面注释，非空 `windows` 会把余额数字挤掉。
        "windows": [],
        "quotaSources": [{
            "id": "account",
            "label": "签到额度余额",
            "provider": ID,
            "remaining": balance,
            "total": Value::Null,
            "unit": "额度",
            "unlimited": false,
            "used": Value::Null,
        }],
        "detail": {
            "source": "session",
            "balance": balance,
            "activeCredits": active,
            "streak": streak,
            "currentCycleDay": cycle,
            "claimedToday": claimed_today,
            "checkinWindow": {
                "name": "每日签到",
                "used": if claimed_today { 1 } else { 0 },
                "limit": 1,
                "unit": "天",
                "note": window_note,
            },
            "rewards": state.get("rewards").cloned().unwrap_or(Value::Null),
            "nextAvailableAt": next_available,
            "lastClaimedAt": state.get("lastClaimedAt").cloned().unwrap_or(Value::Null),
        },
    });

    // `totalLeft` 必须是数字（渲染器直接格式化它）；读不到余额时干脆不给这个
    // 键，让下游继续落到 `availableView` 分支，而不是显示一个假的 0。
    if let (Some(total), Some(object)) = (balance, report.as_object_mut()) {
        object.insert("totalLeft".to_string(), json!(total));
    }
    report
}

/// `GET /api/v1/credits`（Bearer API key）：`creditsRemaining` +
/// `paidInputTokensRemaining`。不依赖浏览器会话，所以能在 Cookie 失效时兜底。
async fn fetch_credits(key: &str) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(UA)
        .build()
        .map_err(|error| format!("构造 HTTP 客户端失败: {error}"))?;
    let response = client
        .get(format!("{BASE}/api/v1/credits"))
        .header("accept", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .send()
        .await
        .map_err(|error| format!("请求 /api/v1/credits 失败: {error}"))?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|error| format!("读取 /api/v1/credits 响应失败: {error}"))?;
    if status == 401 {
        return Err("API key 无效或已撤销".to_string());
    }
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status} {}", truncate(&text, 160)));
    }
    let body: Value =
        serde_json::from_str(&text).map_err(|error| format!("返回的不是 JSON: {error}"))?;

    let credits = body
        .get("creditsRemaining")
        .or_else(|| body.get("credits_remaining"))
        .and_then(Value::as_f64);
    let tokens = body
        .get("paidInputTokensRemaining")
        .or_else(|| body.get("paid_input_tokens_remaining"))
        .and_then(Value::as_f64);

    let mut report = json!({
        "plan": "Jev AI 额度（API key 读取）",
        // 同 `report_from_checkin`：余额列先判 `windows`（必须为空）再判
        // `totalLeft`，所以这两个键都要照顾到。
        "available": credits,
        "availableView": credits.map(|value| format!("{value:.0} 额度")),
        "unit": "额度",
        "balance": credits,
        "credits": credits,
        "quotaSources": [{
            "id": "account",
            "label": "额度余额",
            "provider": ID,
            "remaining": credits,
            "total": Value::Null,
            "unit": "额度",
            "unlimited": false,
            "used": Value::Null,
        }],
        "windows": [],
        "detail": {
            "source": "apiKey",
            "creditsRemaining": credits,
            "paidInputTokensRemaining": tokens,
        },
    });
    if let (Some(remaining), Some(object)) = (credits, report.as_object_mut()) {
        object.insert("totalLeft".to_string(), json!(remaining));
    }
    Ok(report)
}

// ── 登录方式 ──────────────────────────────────────────────────

/// `auth.methods` 的返回（网关 3.5 起读它渲染「添加账号」的登录方式）。
///
/// `type: "api"` = 填写凭证路线：`prompts` 里的键会原样进
/// `account.normalize` 的 `Manual.payload`，所以这里用 `cookie` 与 `name`
/// 两个键，正好对上 `normalize_account` 的解析。
fn login_methods() -> Value {
    json!({ "methods": [
        {
            "methodIndex": 0,
            "type": "api",
            "label": "填写会话 Cookie",
            "instructions": "先在浏览器登录 jev-ai.pro，然后 DevTools → Network → 任意请求 → 复制 Cookie 里的 jev-ai-session（或直接 Copy as cURL 整条粘进来）。插件会自动续用上游换发的新会话，正常情况下只需要粘贴一次。",
            "prompts": [
                {
                    "key": "cookie",
                    "type": "text",
                    "label": "jev-ai-session",
                    "placeholder": "jev-ai-session=Fe26.2**…（可粘贴整条 cURL）",
                    "secret": true
                },
                {
                    "key": "name",
                    "type": "text",
                    "label": "备注名",
                    "placeholder": "可选，留空用账号昵称",
                    "optional": true
                },
                {
                    "key": "apiKey",
                    "type": "text",
                    "label": "API Key（可选，只为看余额）",
                    // 有 key 就能随时读余额（/api/v1/credits），即使签到会话已失效；
                    // 签到本身仍然只认 Cookie。
                    "placeholder": "sk_jev-ai_…（在 jev-ai.pro/jev-api 创建；签到不需要它）",
                    "optional": true
                }
            ]
        }
    ] })
}

// ── 上游调用 ──────────────────────────────────────────────────

/// `GET /api/daily-checkin`：**调用即签到**，返回体同时带余额。
///
/// 第二个返回值是上游这次下发的新会话 Cookie（`Set-Cookie: jev-ai-session=…`）。
/// 上游用的是 rolling 会话：每次成功的响应都会换发一个新的密封体，旧的随即作废。
/// 所以调用方**必须**把它写回账号，否则下一次调用就是 401 —— 这条实测过：
/// 一次成功调用之后，同一把 Cookie 再打就变成 `Unauthorized`。
async fn fetch_state(cookie: &str) -> Result<(Value, Option<String>), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(25))
        // 上游在 Cloudflare 后面：不带浏览器化的头会被拦在业务层之前，
        // 表现为 403 而不是 401（后者才是「Cookie 失效」）。
        .user_agent(UA)
        .build()
        .map_err(|error| format!("构造 HTTP 客户端失败: {error}"))?;

    let response = client
        .get(format!("{BASE}{CHECKIN_PATH}"))
        .header("accept", "*/*")
        .header("accept-language", "zh-CN,zh;q=0.9,en;q=0.8")
        .header("referer", format!("{BASE}/account"))
        .header("sec-fetch-site", "same-origin")
        .header("sec-fetch-mode", "cors")
        .header("sec-fetch-dest", "empty")
        .header("cookie", cookie)
        .send()
        .await
        .map_err(|error| format!("请求 jev-ai.pro 失败: {error}"))?;

    let status = response.status().as_u16();
    // 只在成功响应上认 Set-Cookie：401/403 也会下发一个新密封体（那是给未登录
    // 访客的空会话），把它写回账号等于自己把凭证换成一把无效的。
    let rotated = if (200..300).contains(&status) {
        rotated_cookie(&response)
    } else {
        None
    };
    let text = response
        .text()
        .await
        .map_err(|error| format!("读取上游响应失败: {error}"))?;

    if status == 401 {
        // 返回 Err 而不是某种「失败」档：网关对带 error 的行不排重试
        // （见 `auto_checkin::retry::retry_at` 的 error 短路），死 Cookie
        // 每 30 分钟打一次上游只会刷失败日志。
        return Err(
            "会话 Cookie 已失效或未被接受：请重新登录 jev-ai.pro，复制新的 jev-ai-session 后重新导入"
                .to_string(),
        );
    }
    if status == 403 {
        return Err(format!(
            "上游拒绝请求（HTTP 403，可能是 Cloudflare 挑战）：{}",
            truncate(&text, 160)
        ));
    }
    if !(200..300).contains(&status) {
        return Err(format!("签到失败：HTTP {status} {}", truncate(&text, 160)));
    }

    let state = serde_json::from_str(&text).map_err(|error| format!("上游返回的不是 JSON: {error}"))?;
    Ok((state, rotated))
}

/// 从 `Set-Cookie` 里挑出 `jev-ai-session=…` 那一份（完整 `name=value`）。
fn rotated_cookie(response: &reqwest::Response) -> Option<String> {
    for value in response.headers().get_all(reqwest::header::SET_COOKIE) {
        let Ok(text) = value.to_str() else { continue };
        let Some(start) = text.find("jev-ai-session=") else { continue };
        let rest = &text[start..];
        let end = rest.find(';').unwrap_or(rest.len());
        let pair = rest[..end].trim().to_string();
        if pair.starts_with("jev-ai-session=Fe26.2**") {
            return Some(pair);
        }
    }
    None
}

/// 把上游换发的会话 Cookie 写回账号（条件写入，冲突就放弃）。
///
/// 失败只打日志、不报错：这一次的结果已经拿到了，写不进去的代价是
/// 下一次调用会 401，那时用户会看到「请重新登录」，而不是现在这一次失败。
async fn persist_rotation(ctx: &PluginCtx, found: &agent2api_provider_sdk::StoreAccount, rotated: Option<String>) {
    let Some(pair) = rotated else { return };
    let Some(session) = pair.strip_prefix("jev-ai-session=") else { return };
    let current = found
        .secret
        .get("session")
        .and_then(Value::as_str)
        .unwrap_or("");
    if current == session {
        return;
    }
    let mut secret = found.secret.clone();
    if let Some(object) = secret.as_object_mut() {
        object.insert("session".into(), Value::String(session.to_string()));
        object.insert("cookie".into(), Value::String(pair));
        object.insert("rotated_at".into(), json!(now_ms()));
    }
    if let Err(error) = ctx.store_update(&found.id, &found.secret, &secret).await {
        agent2api_provider_sdk::log_warn!("jev-ai: 会话 Cookie 换发后写回失败: {error}");
    }
}

// ── 结果分档（见模块头的判定表）──────────────────────────────

fn report_of(state: &Value) -> CheckinResult {
    let claimed_today = state
        .get("claimedToday")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let enabled = state.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    let streak = state
        .get("streak")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok());
    let last_claimed_ms = state
        .get("lastClaimedAt")
        .and_then(Value::as_str)
        .and_then(parse_ms);
    let next_available_ms = state
        .get("nextAvailableAt")
        .and_then(Value::as_str)
        .and_then(parse_ms);
    let reward = reward_of(state);
    let now = now_ms();

    let details = Some(json!({
        "streak": streak,
        "currentCycleDay": state.get("currentCycleDay").cloned().unwrap_or(Value::Null),
        "rewards": state.get("rewards").cloned().unwrap_or(Value::Null),
        "balance": state.get("balance").cloned().unwrap_or(Value::Null),
        "activeCredits": state.get("activeCredits").cloned().unwrap_or(Value::Null),
        "nextAvailableAt": next_available_ms,
        "lastClaimedAt": last_claimed_ms,
    }));

    if !enabled {
        return CheckinResult {
            report: proto::CheckinReport {
                outcome: proto::CheckinOutcome::Inactive,
                credit: None,
                streak,
                message: Some("上游当前没有开启每日签到".to_string()),
            },
            retry_at: None,
            details,
        };
    }

    if claimed_today {
        // ① 刚签成：这一次 GET 就是签到动作（上游没有独立的领取路由）。
        if last_claimed_ms.is_some_and(|at| now - at < JUST_CLAIMED_MS) {
            return CheckinResult {
                report: proto::CheckinReport {
                    outcome: proto::CheckinOutcome::Claimed,
                    credit: reward.map(|value| format!("+{value} 额度")),
                    streak,
                    message: None,
                },
                retry_at: None,
                details,
            };
        }
        // ② 今天（北京）已经签过：幂等，别让网关再重试。
        let same_day = match last_claimed_ms {
            Some(at) => beijing_day(at) == beijing_day(now),
            None => false,
        };
        if same_day {
            return CheckinResult {
                report: proto::CheckinReport {
                    outcome: proto::CheckinOutcome::Done,
                    credit: None,
                    streak,
                    message: None,
                },
                retry_at: None,
                details,
            };
        }
        // ③ 上游还是「昨天 UTC 日」的已签状态 —— 窗口未到（北京 08:00 翻页）。
        //    给 retry_at，网关的窗口重试轮会等到那个点再来。
        return CheckinResult {
            report: proto::CheckinReport {
                outcome: proto::CheckinOutcome::Inactive,
                credit: None,
                streak,
                message: Some(match next_available_ms {
                    Some(at) => format!("未到签到窗口，{} 自动重试", beijing_text(at)),
                    None => "未到签到窗口，稍后自动重试".to_string(),
                }),
            },
            retry_at: next_available_ms,
            details,
        };
    }

    // ④ 还没签，但也没到窗口（上游用 `nextAvailableAt` 表达）——继续等。
    if next_available_ms.is_some_and(|at| at > now) {
        return CheckinResult {
            report: proto::CheckinReport {
                outcome: proto::CheckinOutcome::Inactive,
                credit: None,
                streak,
                message: Some(match next_available_ms {
                    Some(at) => format!("未到签到窗口，{} 自动重试", beijing_text(at)),
                    None => "未到签到窗口，稍后自动重试".to_string(),
                }),
            },
            retry_at: next_available_ms,
            details,
        };
    }

    // ⑤ 窗口已开却仍没签上：真失败，交给网关的重试策略。
    CheckinResult {
        report: proto::CheckinReport {
            outcome: proto::CheckinOutcome::Failed,
            credit: None,
            streak,
            message: Some("上游返回 claimedToday=false，签到未生效".to_string()),
        },
        retry_at: None,
        details,
    }
}

/// 今天这一天能拿多少额度：`rewards` 里的第 `currentCycleDay` 项。
fn reward_of(state: &Value) -> Option<i64> {
    let rewards = state.get("rewards")?.as_array()?;
    let day = state
        .get("currentCycleDay")
        .and_then(Value::as_i64)
        .unwrap_or(1);
    let index = usize::try_from(day.saturating_sub(1)).ok()?;
    rewards.get(index).and_then(Value::as_i64)
}

// ── Cookie / 身份 ────────────────────────────────────────────

/// 在 payload 的顶层与常见包装层里找一个非空字符串字段。
///
/// 认的包装层：`inputs`（网关按 `auth.methods` 收集表单时用）、`values`、
/// `fields`。字段名本身由调用方给。这样无论网关把 prompt 值平铺还是包一层，
/// 都能取到。
fn pick_str(payload: &Value, keys: &[&str]) -> Option<String> {
    let scopes: [Option<&Value>; 4] = [
        Some(payload),
        payload.get("inputs").filter(|value| value.is_object()),
        payload.get("values").filter(|value| value.is_object()),
        payload.get("fields").filter(|value| value.is_object()),
    ];
    for scope in scopes.into_iter().flatten() {
        for key in keys {
            let Some(text) = scope.get(*key).and_then(Value::as_str) else { continue };
            if !text.trim().is_empty() {
                return Some(text.to_string());
            }
        }
    }
    None
}

/// 从用户粘贴的文本里抽出 `jev-ai-session` 的值。
///
/// 接受三种形态（用户实际会贴的东西）：
///   1. `jev-ai-session=Fe26.2**…`
///   2. 一整条 `Cookie: a=1; jev-ai-session=…; b=2`
///   3. 整条 `curl … -b "…" …`（DevTools 的 Copy as cURL）
fn extract_session(input: &str) -> Result<String, String> {
    let text = input.trim();
    if text.is_empty() {
        return Err("Cookie 不能为空".to_string());
    }
    const KEY: &str = "jev-ai-session=";
    if let Some(start) = text.find(KEY) {
        let rest = &text[start + KEY.len()..];
        // 值到 `;`、空白、引号或换行为止（`Fe26.2**…` 内部不含这些字符）。
        let value: String = rest
            .chars()
            .take_while(|ch| !matches!(ch, ';' | '"' | '\'' | ' ' | '\t' | '\r' | '\n'))
            .collect();
        if value.starts_with("Fe26.2**") {
            return Ok(value);
        }
        return Err("jev-ai-session 的值看起来不完整（应以 Fe26.2** 开头）".to_string());
    }
    // 只贴了值本身。
    if text.starts_with("Fe26.2**") && !text.contains(' ') {
        return Ok(text.to_string());
    }
    Err("没有找到 jev-ai-session：请从浏览器 DevTools 的请求头里复制完整的 Cookie，或复制整条 cURL"
        .to_string())
}

fn cookie_of(secret: &Value) -> Result<String, String> {
    if let Some(cookie) = secret.get("cookie").and_then(Value::as_str) {
        if !cookie.trim().is_empty() {
            return Ok(cookie.to_string());
        }
    }
    let session = secret
        .get("session")
        .and_then(Value::as_str)
        .ok_or_else(|| "账号里没有会话 Cookie，请重新导入".to_string())?;
    Ok(format!("jev-ai-session={session}"))
}

fn identity_of(session: &str) -> String {
    format!("jev-{:016x}", fnv1a(session))
}

fn tail_of(text: &str, count: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let start = chars.len().saturating_sub(count);
    chars[start..].iter().collect()
}

fn fnv1a(input: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

// ── 时间 ─────────────────────────────────────────────────────

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// RFC3339 → 毫秒时间戳。
fn parse_ms(text: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|value| value.timestamp_millis())
}

/// 北京时间（UTC+8）的日期，用来判「上游这一天是不是网关这一天」。
fn beijing_day(ms: i64) -> Option<chrono::NaiveDate> {
    let offset = FixedOffset::east_opt(8 * 3600)?;
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|utc| utc.with_timezone(&offset).date_naive())
}

/// 给用户看的时间文案（北京时间）。
fn beijing_text(ms: i64) -> String {
    let Some(offset) = FixedOffset::east_opt(8 * 3600) else {
        return format!("{ms}ms");
    };
    match DateTime::<Utc>::from_timestamp_millis(ms) {
        Some(utc) => utc
            .with_timezone(&offset)
            .format("%m-%d %H:%M")
            .to_string(),
        None => format!("{ms}ms"),
    }
}

fn truncate(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= limit {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(limit).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SESSION: &str = "Fe26.2**abc123*DEF456*payload**sig*mac";

    /// 用户实际会贴的三种形态都要能认出来 —— 这是最容易踩的入口。
    #[test]
    fn extract_session_accepts_pasted_forms() {
        // 1. 只要 `key=value` 这一段
        assert_eq!(extract_session(&format!("jev-ai-session={SESSION}")).unwrap(), SESSION);
        // 2. 整条 Cookie
        assert_eq!(
            extract_session(&format!("sb_landing_page=%2F; jev-ai-session={SESSION}; _ga=GA1.1.1")).unwrap(),
            SESSION
        );
        // 3. Copy as cURL：值被引号包着，后面还有别的参数
        assert_eq!(
            extract_session(&format!("curl 'https://jev-ai.pro/api/daily-checkin' -b 'jev-ai-session={SESSION}' -H 'accept: */*'"))
                .unwrap(),
            SESSION
        );
        // 4. 只贴值本身
        assert_eq!(extract_session(SESSION).unwrap(), SESSION);
    }

    /// 空输入与残值要给出能照着做的提示，而不是笼统失败。
    #[test]
    fn extract_session_rejects_empty_and_truncated() {
        assert!(extract_session("").unwrap_err().contains("不能为空"));
        assert!(extract_session("   \n ").unwrap_err().contains("不能为空"));
        assert!(extract_session("jev-ai-session=").unwrap_err().contains("Fe26.2**"));
        // 别的家的 cookie 不该被当成有效会话
        assert!(extract_session("BDUSS=abc; PTOKEN=def").unwrap_err().contains("没有找到"));
    }

    /// 网关会把 `auth.methods` 收集到的表单值包在 `inputs` 里，
    /// 也可能平铺在顶层；两种都要取到（曾因只读顶层而报「Cookie 不能为空」）。
    #[test]
    fn pick_str_reads_both_flat_and_nested_payloads() {
        let flat = json!({ "cookie": "a" });
        assert_eq!(pick_str(&flat, &["cookie"]).as_deref(), Some("a"));

        let nested = json!({ "methodIndex": 0, "inputs": { "cookie": "b" } });
        assert_eq!(pick_str(&nested, &["cookie"]).as_deref(), Some("b"));

        let values = json!({ "values": { "cookie": "c" } });
        assert_eq!(pick_str(&values, &["cookie"]).as_deref(), Some("c"));

        // 空串/空白不算命中，避免把「填了但没填内容」当成有效凭证
        let blank = json!({ "cookie": "   ", "inputs": { "cookie": "d" } });
        assert_eq!(pick_str(&blank, &["cookie"]).as_deref(), Some("d"));
        assert_eq!(pick_str(&json!({}), &["cookie"]), None);
    }

    /// 身份要稳定（同一 Cookie 永远同一条账号记录），且不能把 cookie 明文带出来。
    #[test]
    fn identity_is_stable_and_opaque() {
        let first = identity_of(SESSION);
        assert_eq!(first, identity_of(SESSION));
        assert!(first.starts_with("jev-"));
        assert!(!first.contains("Fe26"), "identity 不能泄漏会话明文：{first}");
        assert_ne!(first, identity_of(&format!("{SESSION}x")));
    }

    /// 余额报告的形状约束，都是前端/网关的硬要求：
    /// `windows` 必须空（否则余额列固定显示「分窗口额度」）、`totalLeft` 必须是数字。
    #[test]
    fn usage_report_shape_matches_gateway_and_ui() {
        let state = json!({
            "claimedToday": true,
            "balance": 27,
            "activeCredits": 7,
            "streak": 1,
            "currentCycleDay": 1,
            "nextAvailableAt": "2026-11-17T00:00:00.000Z",
            "lastClaimedAt": "2026-11-16T01:30:08.906Z",
        });
        let report = report_from_checkin(&state);
        assert_eq!(report["windows"], json!([]), "非空 windows 会挤掉余额数字");
        assert_eq!(report["totalLeft"], json!(27));
        assert_eq!(report["available"], json!(27));
        assert_eq!(report["unit"], json!("额度"));
        // 签到窗口信息移到了 detail，仍然可读
        assert_eq!(report["detail"]["checkinWindow"]["used"], json!(1));
    }

    /// 读不到余额时不能伪造 0：不写 `totalLeft`，让前端落到后面的分支。
    #[test]
    fn usage_report_omits_total_left_when_balance_missing() {
        let report = report_from_checkin(&json!({ "claimedToday": false }));
        assert!(report.get("totalLeft").is_none(), "缺余额时不应写 totalLeft");
        assert_eq!(report["windows"], json!([]));
    }

    /// 签到分档：六状态里真正会出现的三种，网关按它决定要不要重试。
    #[test]
    fn checkin_outcome_classification() {
        // ① 今天（北京）已签、但不是刚领到 → Done，幂等，别重试
        let done = json!({
            "claimedToday": true,
            "lastClaimedAt": beijing_iso(now_ms() - 6 * 60 * 60 * 1000),
        });
        assert_eq!(report_of(&done).report.outcome, proto::CheckinOutcome::Done);
        assert!(report_of(&done).retry_at.is_none());

        // ② 刚领到（3 分钟内）→ Claimed
        let fresh = json!({
            "claimedToday": true,
            "lastClaimedAt": beijing_iso(now_ms() - 30_000),
        });
        assert_eq!(report_of(&fresh).report.outcome, proto::CheckinOutcome::Claimed);

        // ③ 上游仍挂着「昨天」的已签状态 → Inactive + retry_at（窗口未到）
        let yesterday = json!({
            "claimedToday": true,
            "lastClaimedAt": beijing_iso(now_ms() - 40 * 60 * 60 * 1000),
            "nextAvailableAt": beijing_iso(now_ms() + 60 * 60 * 1000),
        });
        let stale = report_of(&yesterday);
        assert_eq!(stale.report.outcome, proto::CheckinOutcome::Inactive);
        assert!(stale.retry_at.is_some(), "窗口未到必须给 retry_at 让网关等到点");

        // ④ 上游关掉签到 → Inactive，且不需要重试
        let disabled = json!({ "claimedToday": false, "enabled": false });
        let off = report_of(&disabled);
        assert_eq!(off.report.outcome, proto::CheckinOutcome::Inactive);
        assert!(off.retry_at.is_none());

        // ⑤ 窗口已开、上游仍回 `claimedToday=false` → Failed（不假装成功）
        let failed = json!({ "claimedToday": false });
        let broke = report_of(&failed);
        assert_eq!(broke.report.outcome, proto::CheckinOutcome::Failed);
        assert!(broke.retry_at.is_none());
    }

    /// 测试辅助：把毫秒时间戳写成上游那种 ISO 串。
    fn beijing_iso(ms: i64) -> String {
        chrono::DateTime::from_timestamp_millis(ms)
            .map(|value| value.to_rfc3339())
            .unwrap_or_default()
    }
}
