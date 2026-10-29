//! AutoClaw（智谱 autoglm）插件实现：**无状态转发 + 双地区**。
//!
//! ── 上游长什么样（移植来源：`autoclaw-local-proxy` + 桌面端 `app.asar`）──
//! AutoClaw 是官方桌面客户端（Electron），它自己就是"代理"：客户端把
//! OpenAI 兼容请求打到 `{upstream}/chat/completions`，头里带一套客户端指纹
//! （`X-Product` / `X-Client-Type` / `X-Tm` …）与 `X-Authorization`。因此
//! 本插件的形态是**无状态转发**：一次 HTTP 请求 = 一次生成。
//!
//! ── 四道上游闸（每一条都是实测出来的，改之前先读对应模块）──────
//! ```text
//!   1. `max_tokens` 小正整数（<10000）→ 406 空响应体（#58 / #69）
//!      归一在 `normalize_max_tokens`，只对本家生效。
//!   2. system 首句指纹 → 406 / 403（2026-09-22 起）
//!      `prompt::normalize` 前置 OpenClaw 身份前缀 + 改写外来身份句。
//!   3. `X-Harness-Type: zcode` → 403 pay-view / 406
//!      转发头里**不发**这个头（刷新头里保留，两处口径不同且都有实测依据）。
//!   4. 请求体带 `metadata` → 500 `parse response failed`（issue #388）
//!      出站前删掉整个 `metadata`（见 `strip_client_metadata`）。
//! ```
//!
//! ── 两个地区是两个 provider（与网关内置版同构）────────────────
//! `autoclaw`（国内版）/ `autoclaw-intl`（国际版）：同一套协议、两个站点，
//! 账号集合与模型目录各自独立。插件注册表里它们是**两个实例**（`Region`
//! 参数化），与 Cline 的两个额度池同一手法。
//!
//! ── 凭证来源四条，优先级由 [`credentials::snapshot_for`] 决定 ─────
//! ```text
//!   账号记录（手动添加 / 网页登录）  → 记录里的 token（`enc:` 自动解密）
//!   桌面端实时登录态                → %APPDATA%/AutoClaw/auth.json（DPAPI 解密）
//!   网关配置                        → ~/.openclaw-autoclaw/openclaw.json（明文）
//!   环境变量                        → AUTOCLAW_TOKEN（国际版 AUTOCLAW_INTL_TOKEN）
//! ```
//! 桌面端两条来源**按设计不回写**（safeStorage 会写坏用户登录态，且服务端
//! 轮换 refresh_token 会与桌面端自己顶掉）；只有账号记录来源回写。
//!
//! ── 从网关搬来时改了什么（逐条对照，便于 parity 核对）────────────
//! ```text
//!   不变：三道闸、请求头集合、模型路由五档判定、错误分类、刷新链（单飞 +
//!         400002 降级重试）、积分 / 订阅 / 签到 / 活动福利的全部判定
//!   改：  ① 凭证从 `store.autoclaw_account_record()` 改成 `store::record()`
//!         ② 目录缓存不落盘（网关自己缓存插件返回的清单）
//!         ③ 自定义模型那一档改为回落默认路由（插件看不到网关的用户配置）
//!         ④ 登录**也搬进来了**（`login.rs` 短信 / `oauth.rs` 两跳 /
//!            `callback_server.rs` 登记端口 / `web_login.rs` 流程表）：网关侧
//!            不再有任何「怎么以官方客户端形态换 token」的知识
//! ```
//!
//! ── 两个地区的登录方式不同（都是唯一入口）─────────────────────
//! ```text
//!   国内版  手机号 + 短信验证码（两次同步调用，管理动作）
//!   国际版  Zai / Google OAuth（前端先跑阿里云验证码，插件借登记端口收回调）
//! ```
//! 详见 `login.rs` / `web_login.rs` 的模块头。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 本 crate 编译期禁止 unwrap/expect/panic（见 `main.rs` 的 deny）。

pub mod balance;
pub mod callback_server;
pub mod catalog;
pub mod checkin;
pub mod credentials;
pub mod login;
pub mod models;
pub mod oauth;
pub mod profile;
pub mod promotion;
pub mod prompt;
pub mod refresh;
pub mod region;
pub mod web_login;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ChatPlan, PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::Value;

use crate::errors::GatewayError;
use crate::headers::Headers;
use crate::logging;

use credentials::{AutoClawCredentials, CredentialOrigin};
use region::Region;

// provider id 的唯一事实来源在 `region`（`PROVIDER_ID_CN` / `PROVIDER_ID_INTL`）：
// 插件侧没有 `ProviderKind` 枚举，`Region::provider_id()` 就是那个映射点。

/// 客户端版本号（源实现 `createUpstreamClient` 的 `desktopAppVersion` 默认值；
/// `server.mjs` 从不覆盖它，所以这里是常量而不是配置项）。
///
/// 它进 `X-Version` 头：上游按它识别 AutoClaw 客户端版本，改动会让上游按不同
/// 的兼容路径解释请求。
const DESKTOP_APP_VERSION: &str = "1.17.8";

/// 上游语言 / 通道（源实现的 `lang` / `channel` 默认值，`server.mjs` 同样不覆盖）
const CLIENT_LANG: &str = "zh-CN";
const CLIENT_CHANNEL: &str = "official";

/// 客户端透传头：`(上游头名, 客户端头名)`（源 `modelProxyUpstreamHeaders` 的表）。
///
/// 客户端带了才发（`typeof value === 'string' && value.trim()`），没带就整条不发
/// —— 这三个是 ZCode/Agent 会话的关联 id，空值发上去会让上游按「有会话」处理。
const PASSTHROUGH_HEADERS: &[(&str, &str)] = &[
    ("X-Session-Id", "x-autoclaw-session-id"),
    ("X-Agent-Id", "x-autoclaw-agent-id"),
    ("X-ZCode-Invocation-Id", "x-autoclaw-zcode-invocation-id"),
];

/// AutoClaw 适配器：**一份实现，两个实例**（各持一个地区）。
///
/// ── 为什么实例持有地区（与网关 `ClineAdapter` 持 `Pool` 同一手法）──
/// `Provider` 的契约里没有任何方法带 provider 参数 —— 实例**就是**那一家的
/// 代表。两个地区是两家 provider，因此各有一个实例，实例上的 `region` 决定它
/// 读哪组域名、哪格目录缓存、哪套环境变量前缀。
///
/// 反面做法是「一个实例 + 每个方法自己判断地区」：那样每个方法都要回答
/// 「我是谁」，而答案只能从入参里猜 —— 转发路径拿得到 account、余额路径只拿
/// 得到 account_id、模型清单路径**什么都拿不到**。持有地区之后，`models()`
/// 这类无参方法也能给出正确答案。
pub struct AutoClawProvider {
    region: Region,
}

impl AutoClawProvider {
    /// 国内版实例（provider id `autoclaw`）。
    pub const CN: Self = Self { region: Region::Cn };
    /// 国际版实例（provider id `autoclaw-intl`）。
    pub const INTL: Self = Self { region: Region::Intl };
}

/// 申报的登录方式（**两个地区完全不同，各只有一条**）。
///
/// 国内版只有手机验证码（它的登录页不渲染 OAuth 按钮，已核对构建产物）；
/// 国际版只有 Zai / Google OAuth（手机表单被死代码消除）。这是实现结论，
/// 不是取舍 —— 详见 `login.rs` 的模块头。
fn login_methods(region: Region) -> Vec<proto::LoginMethod> {
    match region {
        Region::Cn => vec![proto::LoginMethod::Sms {
            hint: Some("用绑定的手机号登录：点「获取验证码」后填入即可".to_string()),
        }],
        Region::Intl => vec![proto::LoginMethod::Browser {
            button: "网页登录（Zai / Google）".to_string(),
            hint: Some(
                "点按钮后先过一次滑块验证（官方风控），随后打开登录页，登录完成即自动添加账号"
                    .to_string(),
            ),
        }],
    }
}

impl Provider for AutoClawProvider {
    fn meta(&self) -> ProviderMeta {
        let region = self.region;
        ProviderMeta::new(
            region.provider_id(),
            format!("AutoClaw {}", region.label()),
        )
        .with_capabilities(proto::Capabilities {
            chat: true,
            stateful: false,
            // 有刷新（`POST /userapi/v1/refresh`，签名失败降级 `agent-refresh`）
            refresh: true,
            // 有远程目录（`GET .../proxy/autoclaw-model-config`）
            model_refresh: true,
            // 积分钱包 + 订阅信息
            usage: true,
            // 每日签到（`autoclaw-task-complete` 的 `daily_signin`）
            checkin: true,
            // 活动福利是"登录即发"，没有领取按钮 —— 与内置版同一结论
            claims: false,
            // 网页登录随实现搬进插件：国际版 OAuth（`web_login` + 登记端口
            // `callback_server`），国内版没有网页授权（上游 OAuth 开关是关的）
            web_login: region == Region::Intl,
            // 上游回自己的路由名，要改写成客户端请求的名字
            sse_model_rewrite: true,
            // 上游把每帧都拼成 `{role, content}`，纯换行的节拍帧要丢
            sse_strip_newline_chunks: true,
            // 有环境变量旁路（`AUTOCLAW_TOKEN`）：账号为空时仍可用默认登录态
            anonymous_default_session: true,
            system_proxy_when_unset: false,
            refresh_uses_account: true,
            // 本家没有「默认模型」概念：不指定模型时应由它自己的路由层回落
            supports_default_model: false,
            // `AUTOCLAW_TOKEN` 旁路实现存在
            env_credentials: true,
        })
        .with_login(login_methods(region))
        .with_models(models::list(region))
        // 账号行：两个地区能力完全一致，差别只在域名 —— 两项都必须申报，
        // 漏了哪一项那一家就掉进兜底（症状：余额入口消失、标识列显示成空）。
        //   `credit_detail` —— 逐钱包余额各一行，另加一条「即将过期」（上游给的是
        //     相对时长文案，原样带出，不换算成时刻）。
        //   `email_as_name` —— 只有国际版：国内版的账号名是手机号 / 昵称。
        //   `expiry` 按两种记录形态都认（插件写 `expiresAt`，旧投影写
        //     `tokenExpiresAt`）。
        .with_account_features(proto::AccountFeatures {
            usage: true,
            credit_detail: true,
            identifier: vec!["userId".to_string()],
            expiry: vec!["expiresAt".to_string(), "tokenExpiresAt".to_string()],
            email_as_name: region == Region::Intl,
            ..Default::default()
        })
        .with_checkin(proto::CheckinInfo {
            description: Some(match region {
                Region::Cn => "官方客户端的每日签到任务".to_string(),
                Region::Intl => "与国内版同一套任务接口 · 站点不同".to_string(),
            }),
            modes: vec![proto::CheckinMode::Checkin],
            ..Default::default()
        })
    }

    // ── 数据面 ────────────────────────────────────────────────

    /// 构造 `POST {upstreamBaseUrl}/chat/completions`。
    ///
    /// body **透传 + 改写三处**（顺序有意义，最后一道是 `prompt::normalize`）：
    ///   1. `model` 换成 `body_model_id`（剥掉路由前缀的模型 ID）；
    ///   2. `max_tokens` 下限归一（[`normalize_max_tokens`]）；
    ///   3. system 提示规范化（[`prompt::normalize`]）。
    fn build_chat<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ChatRequest,
    ) -> SendFuture<'a, ChatPlan> {
        Box::pin(async move {
            let record = crate::store::record(ctx, self.region.provider_id(), &req.account)
                .await?;
            // ── 取出来先剥 `Bearer ` 前缀（issue #386）────────────────
            // 存储里的 token 可能自带前缀（登录响应原样落库的值就是这样），
            // 而下面拼头时会再加一个 —— 不剥就是 `Bearer Bearer eyJ…`，上游按
            // 空格切分后拿到的凭证不是合法 JWT，恒定 401 `Invalid token`。
            // 判据与 `refresh::signed_auth_headers` 出口那一处同一套（那边覆盖
            // 刷新 / 余额 / 签到 / 目录），两条链路都收口才不会有漏网路径。
            let token = crate::crypto::strip_bearer(&crate::store::pick(
                &record,
                &["token", "accessToken", "access_token"],
            ));
            if token.is_empty() {
                return Err("AutoClaw 账号缺少 accessToken，无法转发".to_string());
            }
            // 客户端请求的模型名 → (路由 ID, body 模型 ID)。客户端没给 model 时
            // `resolve_model_route` 回落到默认路由 `zai_auto`（源实现同款兜底）。
            let requested = req
                .body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("");
            let route = models::resolve_model_route(self.region, requested);
            let client = Headers::from_map(&req.headers);
            let mut headers: Vec<(String, String)> = vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                // 源实现给的是 `Accept: */*`（LLM 代理两种响应形态都可能回）
                ("Accept".to_string(), "*/*".to_string()),
            ];
            headers.extend(brand_headers());
            headers.push((
                // 认证头名照抄源实现（`X-Authorization`，不是 `Authorization`）
                "X-Authorization".to_string(),
                format!("Bearer {token}"),
            ));
            headers.push(("X-Request-Id".to_string(), crate::ids::uuid_v4()));
            headers.push(("X-Request-Model".to_string(), route.route_model_id.clone()));
            headers.extend(passthrough_session_headers(&client));
            let mut out_body = req.body.clone();
            if let Some(object) = out_body.as_object_mut() {
                object.insert(
                    "model".to_string(),
                    Value::String(route.body_model_id.clone()),
                );
            }
            normalize_max_tokens(&mut out_body);
            prompt::normalize(&mut out_body);
            // 最后一道：删掉客户端自带的 `metadata`（第四道闸，issue #388）。
            // 放在 prompt::normalize 之后，因为它只动 `messages`，两者互不干扰。
            strip_client_metadata(&mut out_body);
            Ok(ChatPlan::chat(
                format!(
                    "{}/chat/completions",
                    credentials::upstream_base_url(self.region)
                ),
                headers,
                out_body,
            ))
        })
    }

    /// 上游错误分类（逐条对照源实现 `forwardModelRequest`）：
    ///   - 401 → `TokenExpired`（刷新后同账号重试一次）；
    ///   - 429 → `QuotaLimited`（只按 HTTP 状态码；上游不给结构化恢复时间）；
    ///   - 400 等 → 走共用的内容拦截判定；
    ///   - 其余 → `Fatal` 原样透传。
    ///
    /// ── 406 空响应体为什么补一句提示 ────────────────────────────
    /// 406 是上游预校验拒收的统一形态，实测过三个独立方向（`max_tokens` 取值 /
    /// system 首句指纹 / 请求形态风控）。不给提示的话用户只会看到「上游返回
    /// 406: 上游错误」，无从判断该改什么。
    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        let upstream_message = body
            .get("message")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty());
        let raw = upstream_message.unwrap_or("上游错误");
        let mut message = format!("上游返回 {status}: {raw}");
        if status == 406 && upstream_message.is_none() {
            message.push_str(prompt::UPSTREAM_406_HINT);
        }
        if status == 401 {
            return proto::ErrorClass::TokenExpired { message };
        }
        if status == 429 {
            return proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: body.get("code").and_then(Value::as_i64),
                status,
            };
        }
        crate::content_block::classify_or_fatal(
            status,
            body,
            message,
            body.get("code").and_then(Value::as_i64),
        )
    }

    // ── 凭证 ──────────────────────────────────────────────────

    /// 取可用 access token：**凭证快照 + 临期主动刷新**（源实现 `currentCredentials`）。
    ///
    /// 临期窗口是 5 分钟（对齐官方 `DESKTOP_REFRESH_AHEAD_MS`）。账号解析顺序
    /// （源实现 `resolveCredentials`）：指定账号 → 账号组内的当前账号 →
    /// 桌面端实时登录态 → 环境变量，全部收在 [`resolve_credentials`] 里。
    ///
    /// ── 写盘条件 ──────────────────────────────────────────────
    /// 只有**真的刷新了**才回写账号，而且回写走比较-再写（见 [`persist_refresh`]）。
    /// 无条件写盘会让每个请求都整库重写一次账号文件。
    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let previous = resolve_credentials(self.region, ctx, account).await?;
            let refreshed = refresh::ensure_fresh(&previous)
                .await
                .map_err(GatewayError::into_message)?;
            persist_refresh(ctx, &previous, &refreshed).await.map_err(GatewayError::into_control_message)
        })
    }

    /// 401 后的**强制**刷新：不看临期窗口，直接续期。
    ///
    /// 为什么必须覆写默认实现：`ensure_token` 只在「临期」时刷新，而 401 完全
    /// 可能发生在一个时间上还很新的 token 上（服务端侧失效、账号被顶下线、
    /// refreshToken 轮换）。此时只调 ensure 会拿回同一个被拒的 token，编排层
    /// 的「刷新后同账号重试一次」就退化成「用同一个坏 token 再打一次」。
    fn refresh_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let previous = resolve_credentials(self.region, ctx, account).await?;
            let refreshed = refresh::refresh(&previous, true)
                .await
                .map_err(GatewayError::into_message)?;
            persist_refresh(ctx, &previous, &refreshed).await.map_err(GatewayError::into_control_message)
        })
    }

    /// 临期判定：取凭证快照，用凭证自己的 `is_expiring()`（5 分钟窗口）与
    /// `can_refresh()` 判一次 —— 与 `ensure_token` 里 `refresh::ensure_fresh`
    /// 用的是同一对判据。
    ///
    /// ── 为什么必须有 `can_refresh()` ────────────────────────────
    /// 凭证来源里有一条**天然不可刷新**：`openclaw.json` 里只有 access token、
    /// 没有 refreshToken。少了这道判定，这类来源会被维护任务每轮都算成
    /// 「待刷新」，然后稳定失败并往日志里灌错误。
    ///
    /// ── 每小时强制刷新 ────────────────────────────────────────
    /// 除了临期窗口，这里还接上官方那条「每小时无条件刷一次」的语义：目的不是
    /// 等 token 快过期，而是把 refresh_token 温着（服务端会轮换它，长期闲置的
    /// 那一份可能被判失效）。只在本方法做、不在 `is_expiring()` 里做 —— 后者
    /// 被转发链路每请求调用一次，在那里「每小时一次」会退化成「每请求一次」。
    fn token_expiring<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, bool> {
        Box::pin(async move {
            if account.is_empty() {
                return Ok(false);
            }
            let Ok(credentials) = resolve_credentials(self.region, ctx, account).await else {
                return Ok(false);
            };
            if !credentials.can_refresh() {
                return Ok(false);
            }
            Ok(credentials.is_expiring()
                || credentials::hourly_forced_refresh_due(account, logging::now_ms()))
        })
    }

    // ── 目录 ──────────────────────────────────────────────────

    fn models<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async move {
            let region = self.region;
            Ok(proto::ModelList {
                models: models::list(region),
                meta: proto::CatalogMeta {
                    remote: !catalog::remote_models(region).is_empty(),
                    refreshed_at: catalog::last_refreshed_at(region),
                },
            })
        })
    }

    /// 拉取远程模型目录。没有可用登录态时返回 `unchanged()` —— 脚本 / CI 用户
    /// 走环境变量旁路时本就不该刷目录，报红色失败只会让他以为哪里坏了。
    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            let credentials = match resolve_credentials(self.region, ctx, account).await {
                Ok(credentials) => credentials,
                Err(error) => {
                    logging::verbose(
                        "[Models]",
                        &format!("AutoClaw 模型目录刷新跳过：{}", error.message),
                    );
                    return Ok(proto::RefreshOutcome::unchanged());
                }
            };
            Ok(catalog::refresh(&credentials, force).await)
        })
    }

    // ── 余额 / 签到 ───────────────────────────────────────────

    /// 查积分 + 订阅（`balance.rs`：资产钱包 + `subscribe-info` 合成一次调用）。
    ///
    /// 两条链路都在 userapi 域、都带那套 `X-Auth-Sign` 签名头 + Bearer，且源
    /// 实现是裸 fetch（不走账号代理）—— 与刷新接口同一取舍。
    fn quota_sources(&self, report: &Value) -> Vec<Value> {
        crate::quota::sources(&self.meta().id, report)
    }

    fn usage<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, Value> {
        Box::pin(async move {
            balance::query_usage(self.region, ctx, account)
                .await
                .map_err(GatewayError::into_message)
        })
    }

    /// 每日签到（`autoclaw-task-complete` 的 `daily_signin`）+ 活动福利触发。
    fn checkin<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(async move {
            let claim = checkin::claim_daily_signin(ctx, self.region, account)
                .await
                .map_err(GatewayError::into_control_message)?;
            Ok(checkin_report_of(&claim))
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    /// 添加账号：手动粘贴 / 导入本机登录态。
    ///
    /// ── 两条路径都要**出网校验** ────────────────────────────────
    /// 令牌是不透明的（身份在 JWT 声明里，但上游认不认得出网才知道）。
    /// 校验走 `user-info`：它顺带读回 `user_id` / 邮箱 —— 这两样决定账号 id
    /// 与展示名。
    fn normalize_account<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move {
            normalize(self.region, input)
                .await
                .map_err(GatewayError::into_message)
        })
    }

    fn account_view(&self, secret: &Value) -> Result<Value, String> {
        Ok(account_view(self.region, secret))
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(identity_of(self.region, secret))
    }

    // ── 登录 ──────────────────────────────────────────────────

    /// 网页登录（**只有国际版走这条**）。
    ///
    /// 网关的 `login.start` 不带任何 payload（见 `RpcAdapter::login_start_async`），
    /// 而国际版取授权地址**必须先有前端跑完阿里云 SDK 得到的风控参数** —— 因此
    /// 真正的发起走管理动作 `login.oauth-start`（[`Self::admin_action`]）；这里
    /// 只回一条「该去哪儿」的人话，避免界面点了按钮却拿到空地址。
    fn login_start<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _method: &'a str,
        _payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(async move {
            let message = if self.region == Region::Cn {
                "AutoClaw 国内版没有网页授权登录，请改用「手机验证码登录」或「填写凭证」"
            } else {
                "AutoClaw 国际版的网页登录需要先完成一次风控验证，请回到「添加账号」弹窗，\
                 用「网页登录（Zai / Google）」发起"
            };
            Err(message.to_string())
        })
    }

    /// 轮询一次网页登录进度（回调落在插件自己的登记端口上，见 `web_login`）。
    fn login_poll<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async move { web_login::login_poll(flow).await })
    }

    fn login_cancel<'a>(&'a self, _ctx: &'a PluginCtx, flow: &'a str) -> SendFuture<'a, ()> {
        Box::pin(async move { web_login::login_cancel(flow).await })
    }

    /// 管理动作：两条同步登录链 + OAuth 两跳 + 回调地址注入。
    ///
    /// ```text
    ///   login.sms-send     {phone}                        → {deviceId}
    ///   login.sms-verify   {phone, code, deviceId, name?} → NormalizedAccount
    ///   login.oauth-config                                → {enabled, region, prefix, sceneId, supplier}
    ///   login.oauth-start  {vendor, captchaVerifyParam}   → {state, authUrl, warning?}
    ///   login.oauth-callback {state, url}                 → {accepted:true}（远程面板粘贴）
    /// ```
    ///
    /// ── 短信为什么是两次同步调用而不套 `login.start` ─────────────
    /// 国内版唯一的入口就是「发码 → 填码 → 换 token」，没有窗口、没有轮询。
    /// 硬套网页登录框架会让网关为一次短信建后台任务，还要把 deviceId 编进 flow
    /// 再拆出来（见 `login.rs` 的模块头）。
    fn admin_action<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
        action: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            let region = self.region;
            match action {
                "login.sms-send" => {
                    let phone = payload.get("phone").and_then(Value::as_str).unwrap_or("");
                    login::send_code(region, phone)
                        .await
                        .map_err(GatewayError::into_message)
                }
                "login.sms-verify" => {
                    let phone = payload.get("phone").and_then(Value::as_str).unwrap_or("");
                    let code = payload.get("code").and_then(Value::as_str).unwrap_or("");
                    let device_id = payload.get("deviceId").and_then(Value::as_str);
                    let name = payload.get("name").and_then(Value::as_str);
                    let account = login::login_with_code(region, phone, code, device_id, name)
                        .await
                        .map_err(GatewayError::into_message)?;
                    serde_json::to_value(account)
                        .map_err(|error| format!("登录结果无法序列化: {error}"))
                }
                "login.oauth-config" => oauth::captcha_config(region)
                    .await
                    .map_err(GatewayError::into_message),
                "login.oauth-start" => {
                    let vendor_id = payload.get("vendor").and_then(Value::as_str).unwrap_or("");
                    let Some(vendor) = oauth::Vendor::from_id(vendor_id.trim()) else {
                        return Err(format!(
                            "无法识别的登录方式「{vendor_id}」（只支持 zai / google）"
                        ));
                    };
                    if region == Region::Cn {
                        return Err(
                            "AutoClaw 国内版没有网页授权登录（上游的 OAuth 开关是关的）".to_string()
                        );
                    }
                    let captcha = payload
                        .get("captchaVerifyParam")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let (flow, auth_url) = web_login::start_oauth(region, vendor, captcha)
                        .await
                        .map_err(GatewayError::into_message)?;
                    Ok(web_login::start_payload(&flow, &auth_url))
                }
                "login.oauth-callback" => {
                    let flow = payload.get("state").and_then(Value::as_str).unwrap_or("");
                    let url = payload.get("url").and_then(Value::as_str).unwrap_or("");
                    web_login::submit_callback(flow, url).await?;
                    Ok(serde_json::json!({ "accepted": true }))
                }
                other => Err(format!(
                    "AutoClaw 不认识管理动作「{other}」（可用：login.sms-send / login.sms-verify / \
                     login.oauth-config / login.oauth-start / login.oauth-callback）"
                )),
            }
        })
    }
}

// ─── 凭证解析 ──────────────────────────────────────────────────

/// 本次请求的凭证（源实现 `resolveCredentials` 的优先级）。
///
/// 顺序：
///   1. `account_id` 非空 → 该 AutoClaw 账号的记录（桌面端账号实时读 auth.json）；
///   2. `account_id` 为空 → **本地区**组内的当前账号（若有）；
///   3. 都没有 → 桌面端实时登录态；
///   4. 桌面端也读不到 → 本地区的 `{prefix}TOKEN` 环境变量。
///
/// **环境变量排在最后**是与源实现一致的有意取舍：源实现的优先级是「账号列表
/// 选中项 > 环境变量」，所以只有**一个账号记录都没有**时才轮到环境变量。
async fn resolve_credentials(
    region: Region,
    ctx: &PluginCtx,
    account_id: &str,
) -> Result<AutoClawCredentials, GatewayError> {
    let record = match crate::store::record(ctx, region.provider_id(), account_id).await {
        Ok(record) => Some(record),
        Err(_) if account_id.is_empty() => None,
        Err(error) => return Err(error),
    };
    if !account_id.is_empty() && record.is_none() {
        return Err(GatewayError::with_status(
            401,
            format!(
                "AutoClaw {}账号 {account_id} 不存在或不属于该地区（请重新添加）",
                region.label()
            ),
        ));
    }
    credentials::snapshot_for(record.as_ref(), region)
}

/// 刷新结果回写（**只回写手动账号**，且只在真的刷新了的时候）。
///
/// ── 为什么只回写 `AccountStore` 来源 ──────────────────────────
/// 桌面端实时登录态（auth.json / openclaw.json）**按设计不回写**：那是
/// safeStorage 加密格式（回写要用同一把 os_crypt 密钥重新加密，写坏会让用户
/// 桌面端都登不进去），且服务端每次刷新会轮换 refresh_token，网关回写会与桌面端
/// 自己的刷新互相顶掉。
///
/// ── 三个写盘前提 ──────────────────────────────────────────
///   1. **没刷新就不写**：token 与刷新前逐字相同直接返回；
///   2. **只在刷新成功时写**：调用方只在 `Ok` 分支调用本函数；
///   3. **刷新了也要比较-再写**：`store::save` 在同一把账号锁内确认记录里仍是
///      刷新前那份凭证才写入。期间用户重导入 / 删除 / 换号、或另一轮刷新先
///      落地时，旧结果不得覆盖新凭证。
async fn persist_refresh(
    ctx: &PluginCtx,
    previous: &AutoClawCredentials,
    refreshed: &AutoClawCredentials,
) -> Result<String, GatewayError> {
    if refreshed.origin != CredentialOrigin::AccountStore
        || (refreshed.token == previous.token && refreshed.refresh_token == previous.refresh_token) {
        return Ok(refreshed.token.clone());
    }
    let account_id = refreshed.id.as_str();
    let record = crate::store::record(ctx, refreshed.region.provider_id(), account_id).await?;
    // 记录里必须仍是刷新前那份凭证，否则说明别处已经换过（不回写）
    let current_token = crate::store::pick(&record, &["token", "accessToken"]);
    let current_refresh = crate::store::pick(&record, &["refreshToken"]);
    if current_token != previous.token || current_refresh != previous.refresh_token {
        if current_token.is_empty() {
            return Err(GatewayError::with_status(409, "AutoClaw 续期时账号凭据发生变化，请重新登录"));
        }
        return Ok(current_token);
    }
    let mut next = crate::store::secret_view(&record);
    let fields: Vec<(&str, Value)> = vec![
        ("token", Value::String(refreshed.token.clone())),
        (
            "refreshToken",
            Value::String(refreshed.refresh_token.clone()),
        ),
        ("deviceId", Value::String(refreshed.device_id.clone())),
    ];
    let mut fields = fields;
    if let Some(expires_at) = refreshed.expires_at {
        fields.push(("tokenExpiresAt", crate::json::json_number(expires_at)));
    }
    next = crate::store::with_fields(&next, &fields);
    if crate::store::save(ctx, account_id, &crate::store::secret_view(&record), &next).await? {
        return Ok(refreshed.token.clone());
    }
    let latest = crate::store::record(ctx, refreshed.region.provider_id(), account_id).await?;
    let token = crate::store::pick(&latest, &["token", "accessToken"]);
    if token.is_empty() || token == previous.token {
        return Err(GatewayError::with_status(409, "AutoClaw 续期结果未能落盘，请重新登录该账号"));
    }
    Ok(token)
}

// ─── 请求头 ────────────────────────────────────────────────────

/// 品牌头（源实现 `brandHeaders`，逐条照抄 —— **除了** `X-Harness-Type`）。
///
/// `X-Tm` 是平台标识，`X-Version` 是客户端版本。`x_trace_id` 的**下划线写法
/// 也是照抄**（源实现里就是这个键名，与后面那个 `X-Request-Id` 并存）。
///
/// ── 为什么少了 `X-Harness-Type: zcode`（别加回来）────────────
/// 源实现有它，照抄过来后上游自 2026-09-22 起对这个值区别对待：带 `zcode`
/// 稳定 `403 pay-view`（`code 810001`）或 406，不带（或换 `autoclaw`）200 ——
/// 同一账号、同一请求、只改这一个头的实测结论。
fn brand_headers() -> Vec<(String, String)> {
    vec![
        ("X-Product".to_string(), "autoclaw".to_string()),
        ("X-Client-Type".to_string(), "pc".to_string()),
        ("X-Tm".to_string(), platform_tm().to_string()),
        ("X-Version".to_string(), DESKTOP_APP_VERSION.to_string()),
        ("X-Lang".to_string(), CLIENT_LANG.to_string()),
        ("X-Channel".to_string(), CLIENT_CHANNEL.to_string()),
        ("x_trace_id".to_string(), "autoclaw-desktop".to_string()),
    ]
}

/// 平台标识（源实现 `platformTm()`）。
///
/// 上游只认官方桌面端存在的平台：`X-Tm: linux` 会被回 403（`{"message":
/// "forbidden"}`）。实测（固定请求体、只变「账号 × `X-Tm`」）：被上游绑为
/// Windows 的账号收到 `X-Tm: linux` 一律 403，而 `win` / `mac` / 不带均 200。
///
/// 服务端 / Docker 镜像跑在 Linux 上，`cfg!(target_os = "linux")` 恒真，于是每个
/// 出站请求都带 `X-Tm: linux`，被绑为 Windows 的账号经网关调用必然 403。容器里
/// 不存在「真实平台」可选，且官方桌面端只有 Windows / macOS，故 linux 分支回落
/// 为 `win`；macOS 桌面端行为不变。
fn platform_tm() -> &'static str {
    if cfg!(target_os = "macos") {
        "mac"
    } else {
        "win"
    }
}

/// 可选的客户端透传头（源实现 `modelProxyUpstreamHeaders` 末尾那个循环）。
///
/// 客户端带了才发（去空白后非空）；三个头与 transport 无关，OpenAI 协议同样适用。
fn passthrough_session_headers(client: &Headers) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = Vec::new();
    for (upstream_name, client_name) in PASSTHROUGH_HEADERS {
        let value = client
            .get(client_name)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if let Some(value) = value {
            headers.push(((*upstream_name).to_string(), value.to_string()));
        }
    }
    headers
}

/// 出站请求体 `max_tokens` 的**下限归一**（就地修改；#58 / #69 的修复）。
///
/// ── 上游行为 ────────────────────────────────────────────────
/// 上游对出站请求体里的 `max_tokens` **取值**有预校验：30 / 100 / 256 / 512 /
/// 1024 / 8192 这类**小正整数一律拒成 406 空响应体**；**不传该字段**或取值
/// ≥ 10000 都 200。下界落在 8192（拒）与 10000（过）之间，取实测过的安全值
/// 10000。超上界（超过该模型输出上限）上游回 500 —— 那是它自己的语义，本函数
/// **不碰**。`max_completion_tokens` 不参与该校验，**一字不动**。
///
/// ── 归一规则（只在「小正整数」窗口里动手）──────────────────
///   - 字段存在、是正整数、且 < `MAX_TOKENS_FLOOR` → 改写为 `MAX_TOKENS_FLOOR`；
///   - **缺省时不注入**：上游对缺省是 200，凭空注入反而改变行为；
///   - ≥ 下限 / 0 / 负数 / null / 字符串 / 浮点 → **原样**：只归一实测过被拒的
///     「小正整数」形态，其余形态交上游自己报错，不在网关里猜语义。
fn normalize_max_tokens(body: &mut Value) {
    /// 实测过的安全下限：8192（拒）与 10000（过）之间取后者
    const MAX_TOKENS_FLOOR: i64 = 10_000;
    let Some(object) = body.as_object_mut() else {
        return;
    };
    let below_floor = matches!(object.get("max_tokens"), Some(Value::Number(n))
        if n.as_i64().is_some_and(|value| value > 0 && value < MAX_TOKENS_FLOOR));
    if below_floor {
        object.insert(
            "max_tokens".to_string(),
            Value::Number(serde_json::Number::from(MAX_TOKENS_FLOOR)),
        );
    }
}

/// 删掉客户端自带的 `metadata` 字段（第四道上游闸，issue #388）。
///
/// ── 为什么必须删，而不是「转换」────────────────────────────────
/// 上游 `/chat/completions` 收到 `metadata` 时直接回 **500
/// `parse response failed`** —— 注意这不是 4xx 参数错误，而是一个看起来像
/// 「服务端坏了」的 500。于是客户端侧的表现为：调参数、换模型、重试全都没用，
/// 因为坏的是请求体的**形状**而不是内容。issue #388 的实测是「同一完整请求
/// 仅删除 metadata 就恢复 200」，因此这里选择整段删除而不是挑字段改写 ——
/// 上游既然不认这个字段，改写它没有意义，反而可能引入下一个未知闸门。
///
/// ── 这个字段是哪来的 ────────────────────────────────────────
/// ZCode 等客户端为标识设备 / 账号 / 会话，会自动在请求里塞
/// `metadata.user_id`（JSON 字符串）。网关原样透传，插件再原样转发 ——
/// 这条链路上没有任何一环认为它有问题，直到上游 500。
///
/// ── 为什么整段删而不是只删 `user_id` ─────────────────────────
/// `metadata` 是 OpenAI 协议里**可选**的归属字段，对生成结果没有任何影响
/// （上游不用它做计费或路由 —— 真要那样的话它会解析而不是 500）。
/// 只删 `user_id` 会留下一个空对象或其它未知子键，仍然要赌上游认不认；
/// 整段删是唯一确定安全的形态。
///
/// 只影响本家的出站请求体：入站的 `req.body` 是克隆来的，其它家不受影响。
fn strip_client_metadata(body: &mut Value) {
    if let Some(object) = body.as_object_mut() {
        object.remove("metadata");
    }
}

// ─── 账号视图 ──────────────────────────────────────────────────
/// 账号公开视图（界面账号页读的那几个键）。
///
/// ── 为什么邮箱很重要（别删）─────────────────────────────────
/// 国际版的主登录方式是 Zai / Google OAuth，换回来的 token 里**没有邮箱**
/// （JWT 只有 `user_id` / `device_id` / `guid`），界面副标题就只剩上游用户名；
/// 同一个人在两地的账号、或几个同类账号之间分不清。邮箱由 `profile` 模块在
/// 网页登录 / 余额查询时补写到记录里，这里读它。
pub(super) fn account_view(region: Region, secret: &Value) -> Value {
    let token = crate::store::pick(secret, &["token", "accessToken", "access_token"]);
    let email = crate::store::pick(secret, &["email"]);
    let user_id = crate::store::pick(secret, &["userId", "user_id"]);
    let name = {
        let explicit = crate::store::pick(secret, &["name"]);
        if !explicit.is_empty() {
            explicit
        } else if !email.is_empty() {
            email.clone()
        } else if !user_id.is_empty() {
            format!("AutoClaw {}账号 {user_id}", region.label())
        } else {
            format!("AutoClaw {}账号", region.label())
        }
    };
    let desktop = secret
        .get("desktop")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut view = serde_json::json!({
        "name": name,
        "userId": user_id,
        "email": email,
        "tokenTail": crate::store::token_tail(&token),
        "hasSession": true,
        "desktop": desktop,
        "region": region.provider_id(),
        "canRefresh": !crate::store::pick(secret, &["refreshToken"]).is_empty(),
    });
    if let Some(expires_at) = secret.get("tokenExpiresAt").and_then(Value::as_f64) {
        if let Some(object) = view.as_object_mut() {
            object.insert(
                "tokenExpiresAt".to_string(),
                crate::json::json_number(expires_at),
            );
        }
    }
    view
}

/// 账号的稳定身份（去重用）：`user-<userId>`（国际版 `intl-user-<userId>`）。
///
/// ── 为什么国际版要带前缀（账号 id 是整份账号集合的主键）──────────
/// 两地的 userId 空间互不相通，但**同一个人在两地的 userId 完全可能相同**
/// （同一手机号在两套系统里各自注册）。撞了的话存储层的撞 id 保护会直接拒绝
/// 写入 —— 那虽然不会写坏数据，却让「两地账号并存」变成不可能，而并存正是
/// 拆成两个 provider 的意义之一。带上前缀之后两地的 id 天然不相交。
///
/// 国内版必须是裸 `user-`：存量账号的 id 就长这样，换前缀会让它们在升级后
/// 变成「找不到的记录」。
pub(super) fn identity_of(region: Region, secret: &Value) -> String {
    let user_id = crate::store::pick(secret, &["userId", "user_id"]);
    if user_id.is_empty() {
        return String::new();
    }
    // 桌面端账号固定 id（每个地区一条，代表「客户端此刻登录的那个号」）
    if secret
        .get("desktop")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return format!("{}-desktop", region.provider_id());
    }
    format!("{}{user_id}", region.account_id_prefix())
}

// ─── 账号规范化 ────────────────────────────────────────────────

/// 两条添加路径（手动粘贴 / 导入本机登录态）。
///
/// ── 桌面端导入为什么不落 token（与网关内置版同一取舍）──────────
/// 那条记录只标记"这台机器的客户端此刻登录了谁"，转发时实时读 auth.json。
/// 落一份 token 进去反而会让它在客户端换号后变成陈旧凭证。
async fn normalize(
    region: Region,
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, GatewayError> {
    match input.source {
        proto::NormalizeSource::Desktop => {
            let credentials = credentials::local_credentials(region)?;
            let mut secret = serde_json::json!({
                "desktop": true,
                "userId": credentials.user_id,
                "email": credentials.email,
                "deviceId": credentials.device_id,
            });
            if let Some(name) = input
                .name
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                secret["name"] = Value::String(name.to_string());
            }
            Ok(proto::NormalizedAccount {
                public: account_view(region, &secret),
                identity: identity_of(region, &secret),
                secret,
                desktop: true,
            })
        }
        proto::NormalizeSource::FileBase64 { .. } => Err(GatewayError::with_status(
            400,
            "AutoClaw 不支持二进制凭证文件，请改用「导入本机登录态」或粘贴 auth.json",
        )),
        proto::NormalizeSource::FileText { content, .. } => {
            let parsed: Value = serde_json::from_str(content.trim()).map_err(|_| {
                GatewayError::with_status(
                    400,
                    "不是合法 JSON（请选择 AutoClaw 客户端登录后生成的 auth.json）",
                )
            })?;
            from_payload(region, &parsed, input.name.as_deref()).await
        }
        proto::NormalizeSource::Manual { payload } => {
            from_payload(region, &payload, input.name.as_deref()).await
        }
    }
}

/// 从一段凭证载荷建账号（手动粘贴与文件导入共用）。
///
/// 接受两种形态：扁平 `{token|accessToken, refreshToken?, deviceId?}`，以及
/// **直接粘贴 auth.json 内容**（`token` / `refreshToken` 可能是 `enc:` 密文，
/// 由 `credentials::credentials_from_record` 自动解密）。
async fn from_payload(
    region: Region,
    payload: &Value,
    name: Option<&str>,
) -> Result<proto::NormalizedAccount, GatewayError> {
    // 令牌非空是唯一硬前提；其余校验交给 `credentials_from_record`（它认两种
    // 键名、认 `enc:` 密文、认历史记录里写在 `auth` 下的形态）
    if crate::store::pick(payload, &["token", "accessToken", "access_token"]).is_empty() {
        return Err(GatewayError::with_status(
            400,
            "请提供 AutoClaw 的访问令牌（token / accessToken）",
        ));
    }
    let credentials = credentials::credentials_from_record(payload, region)?;
    if credentials.token.trim().is_empty() {
        return Err(GatewayError::with_status(400, "AutoClaw token 解密后为空"));
    }
    // 出网校验：问一次 user-info。它顺带读回 user_id / 邮箱 —— 这两样决定
    // 账号 id 与展示名（拿不到时退回 JWT 声明里的 user_id）。
    let mut user_id = credentials.user_id.clone();
    let mut email = credentials.email.clone();
    let mut device_id = credentials.device_id.clone();
    let mut verified = credentials.clone();
    if verified.device_id.trim().is_empty() {
        verified.device_id = credentials::new_device_id();
        device_id = verified.device_id.clone();
    }
    match profile::fetch(&verified).await {
        Ok(found) => {
            if !found.email.is_empty() {
                email = found.email;
            }
            if let Some(id) = found.user_id {
                if !id.is_empty() {
                    user_id = id;
                }
            }
        }
        Err(error) => {
            // 校验失败 = 这个令牌现在不能用。添加时验证，要的是「现在真的能用」。
            return Err(GatewayError::with_status(
                400,
                format!("AutoClaw 校验失败：{}", error.message),
            ));
        }
    }
    let mut secret = serde_json::json!({
        "token": credentials.token,
        "refreshToken": credentials.refresh_token,
        "deviceId": device_id,
        "userId": user_id,
    });
    if !email.is_empty() {
        secret["email"] = Value::String(email);
    }
    if let Some(name) = name.map(str::trim).filter(|value| !value.is_empty()) {
        secret["name"] = Value::String(name.to_string());
    }
    Ok(proto::NormalizedAccount {
        public: account_view(region, &secret),
        identity: identity_of(region, &secret),
        secret,
        desktop: false,
    })
}

// ─── 签到结果 → 协议档位 ──────────────────────────────────────

/// 把 `{success, msg, reward_points, promotionReward}` 翻成协议档位。
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
    let credit = claim
        .get("rewardPoints")
        .or_else(|| claim.get("reward_points"))
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .map(|value| format!("{value} 积分"));
    let text = message.clone().unwrap_or_default();
    let already = claim
        .get("alreadyCompleted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let outcome = if success {
        proto::CheckinOutcome::Claimed
    } else if already || text.contains("已签") || text.contains("已领") {
        proto::CheckinOutcome::Done
    } else if text.contains("没有可") || text.contains("活动未") {
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
