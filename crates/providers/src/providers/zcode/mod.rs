//! ZCode（智谱 / Z.AI 的编码代理客户端）插件实现：**国内版 / 国际版两个 provider**。
//!
//! ── 上游长什么样 ────────────────────────────────────────────
//! ```text
//!   zcode 平面（登录 / 领取 / 客户端配置）        推理平面
//!   https://zcode.z.ai                     国内 → https://open.bigmodel.cn
//!   （两地相同）                            国际 → https://api.z.ai
//! ```
//!
//! 推理是 **OpenAI 兼容**协议（`{openai_base}/chat/completions`），与
//! raccoon / autoclaw 同构（无状态、Bearer 鉴权、SSE 回写模型名）。
//!
//! ── 两条通道：编码套餐 / 活动套餐（**按账号选**）──────────────
//! | 账号里的 `zcodePlan` | 上游 | 鉴权 | 协议 |
//! |---|---|---|---|
//! | `coding-plan`（默认） | `{openai_base}/chat/completions` | `accessToken` | OpenAI Chat |
//! | `start-plan` | `{zcode}/api/v1/zcode-plan/anthropic/v1/messages` | 套餐 `jwt` | Anthropic Messages |
//!
//! 为什么必须让用户选：**编码套餐与活动套餐是两份独立的额度**（编码套餐到期后
//! 走 OpenAI 通道恒回 429「您的GLM Coding Plan套餐已到期」，而当天领到的活动
//! 额度只能从 `zcode.z.ai` 的 anthropic 端点花掉）。两条通道的 URL / 协议 /
//! 系统提示词要求全在 [`plan`] 里，本文件只放「选哪条」的判据。
//!
//! ── 从网关搬来时改了什么（逐条对照，便于 parity 核对）────────────
//! ```text
//!   不变：请求构造（两条通道的头与体）、错误分类码表、模型清单、思考档位契约、
//!         领取 / 余额 / 端点映射 / 风控配置的全部判定
//!   改：  ① 凭证从 `store.zcode_account_record()` 改成 `store::record()`
//!         ② 设备标识改走 `store.update`（比较再写），逻辑见 `balance`
//!         ③ 配置同步从「常驻循环」改成「挂在异步钩子上」（见 `config_sync`）
//!         ④ **网关自带提示词的开关与正文读不到**（插件没有配置回调）：
//!            恒用网关默认值（装上、用官方原文），见 `plan::build_request`
//!         ⑤ 登录链留在网关（它要开浏览器窗口跑 CLI 轮询）
//! ```
//!
//! ── 三件**已知未做**的事（别误以为已经覆盖）───────────────────
//!   1. **凭证续期**：上游没有续期协议 —— OAuth 令牌用坏了只能重新登录
//!      （参考实现实测：JWT 只带 `iat` 不带 `exp`，过期只以 401 暴露）。
//!      所以 `refresh_token` 如实报错，见下面的说明。
//!   2. **客户端请求签名**（Client Request Signing V4）：参考实现对它全程
//!      fail-open，且实测未签名请求的拒绝理由是鉴权而不是签名 —— 先不做。
//!   3. **活动套餐通道的人机验证令牌**：由界面（WebView 里的阿里云 SDK）铸造、
//!      推进 [`captcha`] 的池子。headless 部署铸不出令牌，那条通道在那里不可用。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 本 crate 编译期禁止 unwrap/expect/panic（见 `main.rs` 的 deny）。

mod admin;
pub mod activation;
pub mod anthropic_outbound;
pub mod anthropic_shared;
pub mod balance;
pub mod captcha;
pub mod checkin;
pub mod claim;
pub mod coding_key;
pub mod config_sync;
pub mod credentials;
pub mod endpoint_mapping;
mod login;
mod models;
pub mod monitor;
pub mod native_tool;
pub mod oauth;
pub mod plan;
pub mod reasoning;
pub mod region;
mod request_context;
mod system_prompt;

use std::collections::BTreeMap;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{ChatPlan, PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::Value;

use crate::errors::GatewayError;
use crate::logging;

use region::Region;

/// 账号记录上的「用哪条通道」字段名（界面读同一个名字）。
///
/// 为什么带 `zcode` 前缀而不是光叫 `plan`：账号记录是**各家共用**的一张形状，
/// `plan` 这个词在别家（Cline 的套餐、Trae 的 planType）都有各自含义，裸名迟早
/// 撞车；带前缀之后前端、存储、适配器三处读的是同一个字符串。
pub const PLAN_FIELD: &str = "zcodePlan";

/// 编码套餐通道（默认值）：开放平台的 OpenAI 兼容端点 + `accessToken`
pub const PLAN_CODING: &str = "coding-plan";

/// 活动套餐通道：`zcode.z.ai` 的 Anthropic 端点 + 套餐 `jwt`（见 [`plan`]）
pub const PLAN_START: &str = "start-plan";

/// 归一化界面/接口给的通道取值：只认上面两个常量，其余（含空值）返回 None。
pub fn normalize_plan(raw: &str) -> Option<&'static str> {
    match raw.trim() {
        PLAN_CODING => Some(PLAN_CODING),
        PLAN_START => Some(PLAN_START),
        _ => None,
    }
}

/// 账号用哪条通道。缺失/认不出 = [`PLAN_CODING`]（存量账号的默认）。
pub fn plan_of(account: &Value) -> &'static str {
    account
        .get(PLAN_FIELD)
        .and_then(Value::as_str)
        .and_then(normalize_plan)
        .unwrap_or(PLAN_CODING)
}

/// 通道的展示名（账号页的下拉、变更提示、错误文案共用这一处措辞）。
///
/// Start Plan 这条叫「**套餐池**」而不是早期的「活动套餐」：那个池子里除了官方
/// 限时活动发放的额度，还有**新注册用户的免费体验额度** —— 叫「活动套餐」会让
/// 恰恰最需要这条通道的用户以为与己无关。
pub fn plan_label(plan: &str) -> &'static str {
    match plan {
        PLAN_START => "套餐池（Start Plan）",
        _ => "编码套餐（Coding Plan）",
    }
}

/// 套餐池请求的网关提示词开关说明。
pub const OFFICIAL_PROMPT_NOTE: &str = "套餐池默认按三块装配：身份句、稳定段、动态段；客户端系统正文合入已有的首条用户消息，不追加系统块或独立消息，也不会因此改成子代理。手动编辑的段优先于默认模板，未编辑的段随插件更新。关闭开关后只发送客户端正文，能否被接受取决于上游规则。3012 不等于额度耗尽，可能与请求兼容性或账号风控有关。";

// ── 「插件读网关用户配置」的两个约定键（`settings.get` 的 `key`）──────────
// ZCode 的官方三段提示词（正文 + 说明）全在本 crate 里，但「装不装」
// 「改过哪几段」仍是**网关的用户配置**（设置页那一栏）。插件通过这两个键读
// 同一份配置，网关侧的回答在 `plugin_host::bootstrap` 里（它认得这两个名字，
// 不会去查 `plugin.<provider>.<key>`）。

/// 「网关自带提示词」的开关（缺失 = 装，与网关侧的默认值一致）。
pub const SETTING_PROMPT_ENABLED: &str = "gatewayPrompt";

/// 「网关自带提示词」的正文覆盖（`{identity, stable, dynamic}`，只含改过的段）。
pub const SETTING_PROMPT_TEXT: &str = "gatewayPromptText";

/// ZCode 适配器（无状态；地区是唯一的状态，构造期固定）。
pub struct ZcodeProvider {
    /// 本实例服务的地区
    region: Region,
}

impl ZcodeProvider {
    /// 国内版实例
    pub const CN: Self = Self { region: Region::Cn };
    /// 国际版实例
    pub const INTL: Self = Self { region: Region::Intl };

    /// 推理基址（`ZCODE_OPENAI_BASE_URL` / `ZCODE_INTL_OPENAI_BASE_URL` 可覆盖）。
    ///
    /// 留覆盖口子是因为上游域名会变（智谱历史上换过编码套餐的域名），而发版
    /// 节奏跟不上域名变更时，用户至少能自己改环境变量救急。
    fn openai_base_url(&self) -> String {
        self.region
            .env_override("OPENAI_BASE_URL")
            .unwrap_or_else(|| self.region.openai_base_url().to_string())
    }
}

impl Provider for ZcodeProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(self.region.provider_id(), format!("ZCode {}", self.region.label()))
            .with_capabilities(proto::Capabilities {
                chat: true,
                stateful: false,
                // 上游没有续期协议（见模块头第 1 条）—— 声称支持会每轮白跑
                refresh: false,
                // 静态模型表，没有远程目录
                model_refresh: false,
                // 套餐余额（billing + 监控两条通道）
                usage: true,
                // 每日签到 = 领当天那份赠送套餐（见 `checkin` 模块头）：
                // 上游每天下发一份带日期段的新套餐，点一下就领 —— 那就是本家的签到
                checkin: true,
                claims: true,
                // 服务端中介的 CLI 轮询：授权地址由上游给、回调在上游域名终结，
                // 因此插件声明「轮询型」由网关发起并轮询（见 `login.rs`）
                web_login: true,
                // 编码套餐的网关会回自己的内部模型名，要改回客户端请求的名字
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
            .with_login(vec![proto::LoginMethod::Browser {
                button: "网页登录".to_string(),
                hint: Some("打开 ZCode 授权页，完成后自动收下账号".to_string()),
            }])
            .with_models(models::list(self.region))
            // 活动套餐通道的人机验证令牌池：池子本体在本模块（`captcha`），
            // 界面只负责铸令牌并推进来
            .with_admin_panels(vec![proto::AdminPanel::CaptchaPool])
            // 账号行：两个地区都要申报。
            //   `usage` 对应 `balance` —— 它是一条**候选令牌链**：有套餐 JWT 时读
            //     billing 网关的 `/zcode-plan/billing/balance`（余额桶 + 到期），
            //     否则用 accessToken 读开放平台监控接口的窗口限额（每 N 小时 /
            //     每周的剩余比例）。两把凭证都没有时回可识别的「未配置」，余额
            //     列显示成中性提示而不是一片红，所以入口照样给。
            //   `claim` 就是那颗「领套餐」：活动期是**每天一份新套餐**（plan_id
            //     带日期段），领过之后当天显示「今日已领」、次日自动恢复。
            //   `plan_channel` 是「使用哪个套餐」设置：编码套餐与活动套餐是两份
            //     独立额度，「套餐已到期」这类拒绝只由其中一条给出，所以它是
            //     设置项而不是自动探测（见 `plan` 的模块头）。
            //   `credit_detail` —— 逐额度桶一行（总量 / 已用 / 剩余，认得出归属时
            //     还带套餐名）；纯比例窗口只有 `remainingPercent`、三个数全缺，
            //     整行跳过 —— 那一档和没有入口一样。
            .with_account_features(proto::AccountFeatures {
                usage: true,
                credit_detail: true,
                claim: true,
                plan_channel: true,
                edition: true,
                identifier: vec!["userId".to_string()],
                expiry: vec!["expiresAt".to_string()],
                ..Default::default()
            })
            // 签到能力申报：本家**有**每日签到 —— 就是「领当天那份赠送套餐」
            // （见 `checkin` 模块头）。上游每天下发一份带日期段的新套餐，点一下
            // 就领，那一动作就是这一家的签到。
            //
            // 同时申报 `plans: true`：签到中心「活动福利」那张卡也按这一位列
            // 账号（见 `api::checkin_center` 的 `plan_rows`），两处展示的是同一
            // 批账号、同一份活动。
            .with_checkin(proto::CheckinInfo {
                description: Some(format!(
                    "每日赠送套餐领取（每天 19:00 起可领）· {}",
                    self.region.label()
                )),
                // 只申报 `Checkin` 一档：签到中心按它给一颗「签到」按钮。
                // 不再申报 `Claim` —— 那一档也是「探测 + 领取」，与本家的签到
                // 是同一个动作，两颗按钮并排只会让用户以为它们不一样。
                modes: vec![proto::CheckinMode::Checkin],
                // 参与每日签到：定时任务按 `core::auto_checkin::times` 的 19:01
                // 触发（那个时刻与 `checkin::daily_window_start` 是同一份事实）。
                daily: true,
                plans: true,
                ..Default::default()
            })
    }

    /// 映射上绑定的思考等级作为**默认档**注入（客户端自己点名了就让位）。
    ///
    /// ── 为什么只接 GLM-5.2 / 5.3 家族 ─────────────────────────
    /// 有依据的只有这两个家族：5.3 是官方目录 + 实测（上游只认 `low` / `high` /
    /// `max`），5.2 是智谱开放文档给出的完整兼容映射。别的模型一律 `Skip` ——
    /// 通用 8 档表里没有「它们也收这个字段」的证据，塞一个上游不认识的键不叫
    /// 生效，只是把未知参数推给上游；而猜错档位的代价是把一条本来能用的请求
    /// 打成 400。
    ///
    /// ── 「客户端指定过」的判据 ────────────────────────────────
    /// 键**在场**就算指定过（哪怕值是 null / 空串 / 一个本家不认的自定义值）：
    /// `apply_to_chat` 对认不出的值会按官方默认档发 `max`，此时再注绑定档会把
    /// 用户的取值**悄悄换成另一个档位**，而请求照样成功、用户看不出区别。
    fn reasoning_patch(&self, level: &str, model: &str, body: &Value) -> proto::ReasoningPatch {
        if body.get(reasoning::EFFORT_FIELD).is_some()
            || body
                .pointer("/thinking/type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("disabled"))
        {
            return proto::ReasoningPatch::Skip {
                reason: "客户端请求体里已指定思考参数，绑定不覆盖".to_string(),
            };
        }
        if !reasoning::is_glm53(model) && !reasoning::is_glm52(model) {
            return proto::ReasoningPatch::Skip {
                reason: "该模型没有已确认的 ZCode 思考档位（目前只有 GLM-5.2 / 5.3 家族）"
                    .to_string(),
            };
        }
        // 表外等级（界面的「自定义输入」）不注入：上游收不收没有证据。
        if crate::reasoning::effort_rank(level).is_none() {
            return proto::ReasoningPatch::Skip {
                reason: "该等级不在通用档位表内（自定义等级不参与转发）".to_string(),
            };
        }
        match reasoning::target_effort(model, Some(level)) {
            Some(value) => proto::ReasoningPatch::Set {
                field: reasoning::EFFORT_FIELD.to_string(),
                value: Value::String(value.to_string()),
            },
            None => proto::ReasoningPatch::Skip {
                reason: "该等级没有可翻译的 ZCode 目标值".to_string(),
            },
        }
    }

    // ── 数据面 ────────────────────────────────────────────────

    /// 构造上游请求：按账号的「使用套餐」**二选一**。
    ///
    ///   - `coding-plan`（默认）：`POST {openai_base}/chat/completions`，
    ///     `Authorization: Bearer {accessToken}`，模型名不改写；思考等级由
    ///     [`Self::reasoning_patch`] 注入映射绑定的默认档、再由
    ///     `reasoning::apply_to_chat` 归一后发出。
    ///   - `start-plan`：`POST {zcode}/api/v1/zcode-plan/anthropic/v1/messages`，
    ///     `Authorization: Bearer {jwt}`，OpenAI 体翻成 Anthropic 并装配官方
    ///     系统提示词块 —— 细节全在 [`plan`]，这里只做分派。
    ///
    /// 两条通道的凭证**不能互相替代**（编码套餐认 accessToken、活动套餐认套餐
    /// JWT），走错门的症状是「套餐已到期」这类业务拒绝而不是鉴权失败，所以
    /// 通道选择做成账号级设置、由用户明确指定。
    fn build_chat<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ChatRequest,
    ) -> SendFuture<'a, ChatPlan> {
        Box::pin(async move {
            claim::refresh_app_version_override(ctx).await;
            let account = crate::store::record(ctx, self.region.provider_id(), &req.account)
                .await
                .map_err(GatewayError::into_control_message)?;
            if account.is_null() {
                return Err("没有可用的 ZCode 账号，请先在「账号」页添加".to_string());
            }
            let client_headers = header_map(&req.headers);
            if plan_of(&account) == PLAN_START {
                config_sync::ensure_captcha_config(self.region, account_proxy(&account).as_ref()).await;
                // 网关自带提示词的两项用户配置（设置页那一栏）：插件通过
                // `settings.get` 的约定键读到同一份 —— 否则界面上拨了开关、
                // 实际发出去的请求不变。读不到时按默认值（装、无覆盖）。
                let official_prompt = prompt_enabled(ctx, self.region.provider_id()).await;
                let override_blocks = prompt_blocks(ctx, self.region.provider_id()).await;
                return plan::build_request(
                    self.region,
                    &account,
                    &req.body,
                    &client_headers,
                    official_prompt,
                    override_blocks.as_ref(),
                )
                .map_err(GatewayError::into_control_message);
            }
            coding_plan_request(self.region, &self.openai_base_url(), &account, &req.body)
        })
    }

    /// 推理请求恒为 stream:true；ZCode 的 JSON 响应是业务拒绝，HTTP 200
    /// 也要先读错误体（实测 1005 / exceed quota limit），不能当 SSE 消费。
    fn is_error_response(&self, status: u16, headers: &BTreeMap<String, String>) -> bool {
        if !(200..300).contains(&status) {
            return true;
        }
        headers
            .get("content-type")
            .map(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("application/json")
            })
            .unwrap_or(false)
    }

    /// 上游错误分类。
    ///
    ///   - `401` → `TokenExpired`（编排层会刷新后同账号重试一次；本家没有续期
    ///     协议，`refresh_token` 会如实报错）
    ///   - `429` → `QuotaLimited`（编码套餐是「5 小时 + 每周」双窗口限额，上游
    ///     不给结构化的恢复时间。「套餐已到期」也是这条 —— 上游用 429 表达它，
    ///     而**换通道**才是出路）
    ///   - 业务码 `1005` → `QuotaLimited`（活动套餐可用 HTTP 200 返回）
    ///   - 其余 → 交给共用的内容拦截判定（`content_block`）
    ///
    /// ── 文案的取值链为什么要多一段 `error.message` ──────────────
    /// 活动套餐通道说 Anthropic 协议，它的错误体是
    /// `{"type":"error","error":{"type":"...","message":"..."}}` —— 顶层没有
    /// `message`/`msg`。不多认这一段，那条通道上所有错误都会退化成「上游错误」。
    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        let raw = body
            .get("message")
            .or_else(|| body.get("msg"))
            .or_else(|| body.get("error").and_then(|error| error.get("message")))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            // 上游的 401 是**空体**（实测：`Content-Length: 0`），而这正是活动
            // 套餐通道上最常见的一种失败（套餐 JWT 用坏了）—— 照旧给「上游错误」
            // 四个字，用户完全不知道下一步该做什么。这一档按状态码给一句可执行的话。
            .unwrap_or_else(|| {
                if status == 401 {
                    "凭证被上游拒绝（套餐池通道认套餐 JWT、编码套餐认访问令牌）：\
                     请在「账号」页重新登录该账号"
                } else {
                    "上游错误"
                }
            });
        let code = body.get("code").and_then(Value::as_i64);
        // 3007 是「验证码令牌被拒」的信号：记进令牌池 —— 界面据此把库存立刻
        // 补齐；同时它把「推理免码」的结论翻回「要码」（见 `captcha` 的模块头）。
        if code == Some(3007) {
            captcha::note_challenge_and_require_token();
        }
        // 提示的两条来路：业务码表（`code_hint`）优先；限额档再按**错误原文**
        // 补池子错位的引导（429「余额不足/无可用资源包」通常不是账号没钱，
        // 而是额度记在另一个池子里）。
        let hint = match code.and_then(plan::code_hint) {
            Some(hint) => Some(hint),
            None if status == 429 || code == Some(1005) => plan::quota_hint(raw),
            None => None,
        };
        let message = match hint {
            Some(hint) => format!("上游返回 {status}: {raw}（{hint}）"),
            None => format!("上游返回 {status}: {raw}"),
        };
        if code == Some(3012) {
            return proto::ErrorClass::Fatal {
                status: if (200..300).contains(&status) { 502 } else { status },
                message,
                upstream_code: code,
            };
        }
        if status == 401 {
            return proto::ErrorClass::TokenExpired { message };
        }
        if status == 429 || code == Some(1005) {
            return proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: code,
                status: 429,
            };
        }
        // 被响应头检查拒绝的 2xx JSON 必须产生失败状态；保留原始 HTTP 状态在
        // message 中，未知业务码不猜测为凭证失效或额度用尽。
        let status = if (200..300).contains(&status) { 502 } else { status };
        crate::content_block::classify_or_fatal(status, body, message, code)
    }

    fn retry_decision(
        &self,
        _status: u16,
        body: &Value,
        _attempt: usize,
        _budget: usize,
    ) -> agent2api_provider_sdk::RetryDecision {
        if body.get("code").and_then(Value::as_i64) == Some(3012) {
            return agent2api_provider_sdk::RetryDecision::Stop {
                reason: "上游风控拒绝（3012），停止本次请求，不再原地重发或换账号".to_string(),
            };
        }
        agent2api_provider_sdk::RetryDecision::Default
    }

    // ── 凭证 ──────────────────────────────────────────────────

    /// 取可用令牌：只读账号里的 `accessToken`，**不续期**（见模块头）。
    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            claim::refresh_app_version_override(ctx).await;
            let token = session_access_token(ctx, self.region, account)
                .await
                .map_err(GatewayError::into_control_message)?;
            // 顺带一次「到点没」的配置同步（见 `config_sync::maybe_sync`）
            config_sync::maybe_sync(ctx).await;
            Ok(token)
        })
    }

    /// 401 后的强制刷新：本家尚未实现续期，如实报错让用户重新登录。
    ///
    /// **不要**在这里回落到「再读一次账号」—— 那正是编排层已经做过的动作，
    /// 返回同一个被拒的 token 会让「刷新后重试一次」退化成一次无意义的重复请求。
    fn refresh_token<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, String> {
        Box::pin(async {
            Err("ZCode 的登录态无法自动续期，请在「账号」页重新登录该账号".to_string())
        })
    }

    // ── 目录 ──────────────────────────────────────────────────

    /// 静态模型表（两地共用一份，见 `models.rs` 的模块头）。
    ///
    /// 本家**没有**远程目录：`model_refresh` 恒 false，界面不给这家渲染
    /// 「刷新模型清单」按钮。所以这里不走默认实现的 `meta().default_models`，
    /// 而是按地区现取 —— 两个实例共用一份表，但显式一点更好读。
    fn models<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        _account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async move {
            Ok(proto::ModelList {
                models: models::list(self.region),
                meta: proto::CatalogMeta::default(),
            })
        })
    }

    // ── 余额 / 领取 ────────────────────────────────────────────

    /// 查套餐余额：`balance`（billing 通道）→ `monitor`（窗口限额）的候选链。
    ///
    /// 缺 JWT 且缺 API Key 时回 `usage_not_configured`（400 + 中性标记）——
    /// 前端显示成「未配置查询」而不是失败。
    fn quota_sources(&self, report: &Value) -> Vec<Value> {
        crate::quota::sources(&self.meta().id, report)
    }

    fn usage<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, Value> {
        Box::pin(async move {
            balance::query_usage(ctx, self.region, account)
                .await
                .map_err(GatewayError::into_message)
        })
    }

    /// 每日签到：领当天那份赠送套餐（`checkin::claim_daily_checkin`）。
    ///
    /// 与 `claim_status` / `claim`（手动领取）走同一条上游链路，差别只在
    /// 无人值守的处置：拿不到验证码令牌时给中性结果而不是失败，见 `checkin`
    /// 的模块头。
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

    /// 带 `manual` 的签到：本家没有写请求限流闸，两档执行体一致，只把
    /// `retryAt` 透出去（网关的重试链读它）。
    fn checkin_with_options<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        _manual: bool,
    ) -> SendFuture<'a, agent2api_provider_sdk::CheckinResult> {
        Box::pin(async move {
            let value = checkin::claim_daily_checkin(ctx, self.region, account)
                .await
                .map_err(GatewayError::into_control_message)?;
            Ok(agent2api_provider_sdk::CheckinResult {
                report: checkin_report(&value),
                retry_at: value.get("retryAt").and_then(Value::as_i64),
            })
        })
    }

    /// 领取类活动的只读状态：探测当前可领的套餐（`GET /preview`）。
    ///
    /// 与每日签到是同一份活动（见 `checkin` 模块头），这里给的是「界面上点开
    /// 活动福利卡时看到的清单」。`refresh` 在这家没有意义（预览本来就是实时的），
    /// 签名里保留是为了协议一致。
    fn claim_status<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        _refresh: bool,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            // 刷新「客户端版本号」覆盖（设置页可改，见 `claim::app_version`）。
            // 必须在 `preview` 之前：版本号决定上游下不下发套餐列表。
            claim::refresh_app_version_override(ctx).await;
            let record = crate::store::record(ctx, self.region.provider_id(), account)
                .await
                .map_err(GatewayError::into_message)?;
            let jwt = crate::store::pick(&record, &["jwt"]);
            let device_mid = crate::store::pick(&record, &["deviceMid", "device_mid"]);
            let user_id = crate::store::pick(&record, &["userId", "user_id"]);
            let proxy = account_proxy(&record);
            let mid = (!device_mid.is_empty()).then_some(device_mid.as_str());
            let outcome = claim::preview(self.region, &jwt, mid, proxy.as_ref())
                .await
                .map_err(GatewayError::into_message)?;
            // ── 空列表时补报激活事件再重探（与网关 `api::zcode_claim::preview`
            // 逐条相同）──────────────────────────────────────────────
            // 资格由激活事件发放，而本家不跑官方客户端，那一步不会自动发生。
            // 代价是两次事件 + 一次重探，收益是把「明明有资格却看不到套餐」
            // 变成看得到 —— 预览是用户点出来的低频动作，这个代价付得起。
            // 列表非空时**不报**：探测仍是只读的。
            let mut activated: Option<bool> = None;
            let mut activation_error: Option<String> = None;
            let outcome = match outcome {
                claim::PreviewOutcome::Plans(ref plans) if plans.is_empty() => {
                    if !user_id.is_empty() {
                        match activation::report(
                            self.region,
                            &user_id,
                            &device_mid,
                            proxy.as_ref(),
                        )
                        .await
                        {
                            Ok(()) => activated = Some(true),
                            Err(error) => {
                                logging::log(
                                    "[Claim]",
                                    &format!(
                                        "ZCode {}账号「{account}」激活事件上报失败：{error}",
                                        self.region.label()
                                    ),
                                );
                                activated = Some(false);
                                activation_error = Some(error);
                            }
                        }
                    }
                    match claim::preview(self.region, &jwt, mid, proxy.as_ref()).await {
                        // 重探失败不把首次的空探测降级成错误（首次已给出可信答案）
                        Ok(next) => next,
                        Err(error) => {
                            logging::log(
                                "[Claim]",
                                &format!(
                                    "ZCode {}账号「{account}」补报激活后的重探失败：{}",
                                    self.region.label(),
                                    error.message
                                ),
                            );
                            claim::PreviewOutcome::Plans(Vec::new())
                        }
                    }
                }
                other => other,
            };
            let mut body = preview_json(&outcome);
            if let Some(object) = body.as_object_mut() {
                object.insert(
                    "activated".to_string(),
                    activated.map(Value::Bool).unwrap_or(Value::Null),
                );
                object.insert(
                    "activationError".to_string(),
                    activation_error.map(Value::String).unwrap_or(Value::Null),
                );
            }
            Ok(body)
        })
    }

    /// 领取一个套餐（`claim` 参数是套餐 id；空串 = 领第一个可领的）。
    ///
    /// 人机验证令牌由**界面**铸造后推进 [`captcha`] 的池子，这里只消费。
    fn claim<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        claim_id: &'a str,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            // 领取同样先刷一次版本号覆盖（与 `claim_status` 同口径）：
            // 用户可能刚在设置里填完就直接点了领取，两条路的判定要一致。
            claim::refresh_app_version_override(ctx).await;
            let record = crate::store::record(ctx, self.region.provider_id(), account)
                .await
                .map_err(GatewayError::into_message)?;
            let jwt = crate::store::pick(&record, &["jwt"]);
            let device_mid = crate::store::pick(&record, &["deviceMid", "device_mid"]);
            let proxy = account_proxy(&record);
            // 没点名就现探一次（「一键领取」的语义）
            let plan_id = if claim_id.trim().is_empty() {
                match claim::preview(
                    self.region,
                    &jwt,
                    (!device_mid.is_empty()).then_some(device_mid.as_str()),
                    proxy.as_ref(),
                )
                .await
                .map_err(GatewayError::into_message)?
                {
                    claim::PreviewOutcome::Plans(plans) => plans
                        .first()
                        .map(|plan| plan.plan_id.clone())
                        .unwrap_or_default(),
                    claim::PreviewOutcome::NotDeployed => String::new(),
                }
            } else {
                claim_id.trim().to_string()
            };
            if plan_id.is_empty() {
                return Err("当前没有可领取的套餐".to_string());
            }
            // 验证码令牌：要就要、不要就整个不带（见 `claim` 的模块头）
            let (param, region_tag) = if captcha::required() {
                match captcha::take() {
                    Some(pair) => pair,
                    None => {
                        return Err(
                            "套餐池通道需要人机验证令牌，当前令牌池为空：令牌由**打开着的界面**\
                             自动铸造，请先打开主窗口或浏览器面板页让它补铸"
                                .to_string(),
                        )
                    }
                }
            } else {
                (String::new(), String::new())
            };
            let outcome = claim::claim(
                self.region,
                &jwt,
                &plan_id,
                &param,
                (!region_tag.is_empty()).then_some(region_tag.as_str()),
                (!device_mid.is_empty()).then_some(device_mid.as_str()),
                proxy.as_ref(),
            )
            .await
            .map_err(GatewayError::into_message)?;
            Ok(claim_json(&outcome))
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    /// 添加账号：粘贴访问令牌 / 套餐 JWT（自助登录留在网关，见模块头）。
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
        Ok(identity_of(secret))
    }

    // ── 登录（服务端中介的 CLI 轮询）──────────────────────────

    fn login_start<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        method: &'a str,
        _payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(async move {
            login::login_start(self.region, method)
                .await
                .map_err(GatewayError::into_message)
        })
    }

    fn login_poll<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(async move {
            login::login_poll(self.region, flow)
                .await
                .map_err(GatewayError::into_message)
        })
    }

    fn login_cancel<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        flow: &'a str,
    ) -> SendFuture<'a, ()> {
        Box::pin(async move {
            login::login_cancel(flow);
            Ok(())
        })
    }

    /// 管理动作：限时套餐的「探测 / 风控配置 / 领取」。
    ///
    /// ```text
    ///   claim.preview        探测可领套餐（只读；空列表时补报激活事件再重探）
    ///   claim.captcha-config 取阿里云风控配置（滑块要不要弹）
    ///   claim.claim          领取（验证码参数在 payload 里给）
    /// ```
    ///
    /// ── 返回的是**网关的**形状 ────────────────────────────────
    /// 与 `Provider::claim` 的 `claim_json`（插件内部契约）不同：这三个动作
    /// 对应的前端界面读的是 `api::zcode_claim` 的既有键名（`deployed` /
    /// `failureLabel` / `activated` …）。形状一变界面就白屏，所以逐字复刻，
    /// 不做「统一格式」的整理。
    fn admin_action<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        action: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, Value> {
        Box::pin(async move {
            claim::refresh_app_version_override(ctx).await;
            if let Some(outcome) = admin::machine_action(action, payload) {
                config_sync::maybe_sync(ctx).await;
                return outcome;
            }
            let record = crate::store::record(ctx, self.region.provider_id(), account)
                .await
                .map_err(GatewayError::into_message)?;
            let jwt = crate::store::pick(&record, &["jwt"]);
            let user_id = crate::store::pick(&record, &["userId", "user_id"]);
            let device_mid = crate::store::pick(&record, &["deviceMid", "device_mid"]);
            let device_mid = (!device_mid.trim().is_empty()).then(|| device_mid.clone());
            let proxy = account_proxy(&record);
            match action {
                "claim.captcha-config" => match claim::captcha_config(self.region, proxy.as_ref())
                    .await
                {
                    // `enabled: false` 或配置不全 → 前端不弹滑块
                    Ok(None) => Ok(serde_json::json!({ "enabled": false })),
                    Ok(Some(config)) => {
                        captcha::apply_config(&config);
                        Ok(serde_json::json!({
                            "enabled": config.enabled,
                            "prefix": config.prefix,
                            "sceneId": config.scene_id,
                            "region": config.region,
                            "skipModelRequest": config.skip_model_request,
                        }))
                    }
                    Err(error) => Err(GatewayError::into_message(error)),
                },
                "claim.preview" => {
                    if jwt.trim().is_empty() {
                        return Err(missing_jwt(self.region));
                    }
                    preview_action(
                        self.region,
                        &jwt,
                        device_mid.as_deref(),
                        &user_id,
                        proxy.as_ref(),
                    )
                    .await
                }
                "claim.claim" => {
                    if jwt.trim().is_empty() {
                        return Err(missing_jwt(self.region));
                    }
                    claim_action(
                        self.region,
                        &jwt,
                        device_mid.as_deref(),
                        &user_id,
                        proxy.as_ref(),
                        payload,
                    )
                    .await
                }
                other => Err(format!(
                    "ZCode 不认识管理动作「{other}」（可用：claim.preview / \
                     claim.captcha-config / claim.claim / captcha.push / captcha.stats / prompt.info）"
                )),
            }
        })
    }
}

/// 「网关自带提示词」是否装上（**缺键 = 装**，与网关侧默认值逐字一致）。
///
/// 读不到（插件不在网关里跑、或老网关没有这条回调）时同样回 `true` ——
/// 那是存量行为，也正是「用户没动过设置」时的取值。
async fn prompt_enabled(ctx: &PluginCtx, provider: &str) -> bool {
    match ctx.settings_get(provider, SETTING_PROMPT_ENABLED).await {
        Ok(Value::Bool(flag)) => flag,
        _ => true,
    }
}

/// 用户改过的提示词正文（`{identity, stable, dynamic}`，只含改过的段）。
///
/// 形状不对/读不到一律回 `None`（= 全部用官方原文）：宁可用官方原文，也不要
/// 因为一次解析失败把三段拼成残缺的东西发上去 —— 上游按结构校验，残缺会 3012。
async fn prompt_blocks(
    ctx: &PluginCtx,
    provider: &str,
) -> Option<plan::GatewayBlocks> {
    let value = ctx
        .settings_get(provider, SETTING_PROMPT_TEXT)
        .await
        .ok()?;
    let object = value.as_object()?;
    let mut blocks = plan::GatewayBlocks::default();
    let mut touched = false;
    for field in plan::GatewayBlocks::FIELDS {
        if let Some(text) = object.get(field).and_then(Value::as_str) {
            if !text.trim().is_empty() {
                blocks.set(field, text.to_string());
                touched = true;
            }
        }
    }
    touched.then_some(blocks)
}

/// 「套餐令牌」缺失时的文案（两个入口共用，措辞与网关原文一致）。
fn missing_jwt(region: Region) -> String {
    format!(
        "该 ZCode {}账号没有套餐令牌（jwt），无法领取。请重新用「网页登录」添加，\
         或在账号里补填 Coding Plan JWT",
        region.label()
    )
}

/// 补报激活事件，返回 `(activated, activationError)`。
///
/// 资格由上游的激活事件（`app_launch` / `app_daily_active`）发放，而本家不跑
/// 官方客户端、那一步不会自动发生 —— 所以它是领取的前置动作。
/// 失败不阻断调用方（领取有它自己的路径），原因如实带回响应。
async fn report_activation(
    region: Region,
    user_id: &str,
    device_mid: Option<&str>,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
) -> (Option<bool>, Option<String>) {
    if user_id.trim().is_empty() {
        // 手工粘贴 API Key 建的账号没有 userId：报也归不到名下
        return (None, None);
    }
    match activation::report(region, user_id, device_mid.unwrap_or(""), proxy).await {
        Ok(()) => {
            logging::verbose(
                "[Claim]",
                &format!("ZCode {}账号已上报激活事件", region.label()),
            );
            (Some(true), None)
        }
        Err(error) => {
            logging::log(
                "[Claim]",
                &format!("ZCode {}账号激活事件上报失败：{error}", region.label()),
            );
            (Some(false), Some(error))
        }
    }
}

/// 签到结果（`{success, outcome, msg}` 形状）→ 协议 `CheckinReport`。
///
/// 判据与 Qoder 的 `checkin_report` 同序：`alreadyCompleted` 优先于 `success`
/// （今天已领要说成 `Done` 而不是「刚领到」—— 后者会误导界面播报积分），
/// 中性未领取走 `inactive`（不落账、按 `retryAt` 重试），其余是 `Failed`。
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
        // 收益文案已经是人话（「领到「ZCode 1024 Dev Day」…」），直接透出
        credit: value
            .get("planName")
            .and_then(Value::as_str)
            .map(str::to_string),
        streak: None,
        message: value.get("msg").and_then(Value::as_str).map(str::to_string),
    }
}

/// 一条可领套餐 → 前端 JSON（字段名与网关 `plan_to_json` 逐字相同）。
fn plan_to_json(plan: &claim::ClaimablePlan) -> Value {
    serde_json::json!({
        "planId": plan.plan_id,
        "name": plan.name,
        "description": plan.description,
        "priority": plan.priority,
        "startsAt": plan.starts_at,
        "endsAt": plan.ends_at,
        "entitlements": plan
            .entitlements
            .iter()
            .map(|item| serde_json::json!({
                "showName": item.show_name,
                "unitType": item.unit_type,
                "grantUnits": item.grant_units,
                "period": item.period,
                "effectiveAt": item.effective_at,
            }))
            .collect::<Vec<Value>>(),
    })
}

/// 失败语义 → 前端用的稳定字符串（前端按它选文案，不按中文 label 匹配）。
fn failure_key(failure: claim::ClaimFailure) -> &'static str {
    use claim::ClaimFailure;
    match failure {
        ClaimFailure::NotFound => "not_found",
        ClaimFailure::Unavailable => "unavailable",
        ClaimFailure::AlreadyClaimed => "already_claimed",
        ClaimFailure::Ineligible => "ineligible",
        ClaimFailure::QuotaExhausted => "quota_exhausted",
        ClaimFailure::InvalidRequest => "invalid_request",
        ClaimFailure::Captcha => "captcha",
        ClaimFailure::LoginRequired => "login_required",
        ClaimFailure::HttpError => "http_error",
        ClaimFailure::Unknown => "unknown",
    }
}

/// `claim.preview` 的实现：探测 → （空列表时）补报激活 → 重探。
///
/// ── 一处**有副作用**的分支（别当成纯只读）────────────────────
/// **探测结果为空时**：先补报激活事件、再重探一次 —— 空列表可能只是资格没
/// 登记，而那是本家不跑官方客户端的必然结果。列表非空时不报（读写分开的
/// 口径只在这一处被打破：预览是低频的用户点击动作，代价付得起）。
async fn preview_action(
    region: Region,
    jwt: &str,
    device_mid: Option<&str>,
    user_id: &str,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
) -> Result<Value, String> {
    let outcome = claim::preview(region, jwt, device_mid, proxy)
        .await
        .map_err(GatewayError::into_message)?;
    let (plans, deployed, activated, activation_error) = match outcome {
        claim::PreviewOutcome::NotDeployed => (Vec::new(), false, None, None),
        claim::PreviewOutcome::Plans(plans) if !plans.is_empty() => (plans, true, None, None),
        claim::PreviewOutcome::Plans(_) => {
            let (activated, activation_error) =
                report_activation(region, user_id, device_mid, proxy).await;
            match claim::preview(region, jwt, device_mid, proxy).await {
                Ok(claim::PreviewOutcome::NotDeployed) => {
                    (Vec::new(), false, activated, activation_error)
                }
                Ok(claim::PreviewOutcome::Plans(plans)) => {
                    logging::verbose(
                        "[Claim]",
                        &format!(
                            "ZCode {}账号补报激活事件后重探：{} 份可领套餐",
                            region.label(),
                            plans.len()
                        ),
                    );
                    (plans, true, activated, activation_error)
                }
                // 重探失败不把「刚才那次成功的空探测」降级成错误：
                // 首次探测已给出可信答案（当前没有可领套餐），补报只是加试。
                Err(error) => {
                    logging::log(
                        "[Claim]",
                        &format!(
                            "ZCode {}账号补报激活后的重探失败：{error}",
                            region.label()
                        ),
                    );
                    (Vec::new(), true, activated, activation_error)
                }
            }
        }
    };
    Ok(serde_json::json!({
        "plans": plans.iter().map(plan_to_json).collect::<Vec<Value>>(),
        "deployed": deployed,
        "activated": activated,
        "activationError": activation_error,
    }))
}

/// `claim.claim` 的实现：补报激活 → （没点名就探一次选优先级最高的）→ 领取。
///
/// 失败也返回 `Ok`（业务结果不是 HTTP 错误）：`already_claimed` /
/// `quota_exhausted` / `captcha` 都是正常结果，前端要靠 `failure` 给不同提示。
/// `claimedAt` 由网关落库后回填（插件写不到记录的普通字段），这里先不出现。
async fn claim_action(
    region: Region,
    jwt: &str,
    device_mid: Option<&str>,
    user_id: &str,
    proxy: Option<&agent2api_provider_protocol::WireProxy>,
    payload: &Value,
) -> Result<Value, String> {
    let text = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let captcha_verify_param = text("captchaVerifyParam");
    // 空值**放行**：上游的风控配置可能是「此刻不要验证码」，拦在这里会让
    // 「不需要验证码」的那一刻反而领不了。有没有值交给上游判（要就回 3007）。
    let captcha_region = {
        let value = text("captchaRegion");
        (!value.is_empty()).then_some(value)
    };
    let (activated, activation_error) =
        report_activation(region, user_id, device_mid, proxy).await;
    let requested = text("planId");
    let plan_id = if requested.is_empty() {
        match claim::preview(region, jwt, device_mid, proxy)
            .await
            .map_err(GatewayError::into_message)?
        {
            claim::PreviewOutcome::Plans(plans) => match claim::pick_target(&plans, "") {
                Some(plan) => plan.plan_id.clone(),
                None => {
                    return Ok(serde_json::json!({
                        "ok": false,
                        "failure": "unavailable",
                        "message": "当前没有可领取的套餐",
                        "activated": activated,
                        "activationError": activation_error,
                    }))
                }
            },
            claim::PreviewOutcome::NotDeployed => {
                return Ok(serde_json::json!({
                    "ok": false,
                    "failure": "unavailable",
                    "message": "活动尚未开始（上游接口未部署）",
                    "activated": activated,
                    "activationError": activation_error,
                }))
            }
        }
    } else {
        requested
    };
    let outcome = claim::claim(
        region,
        jwt,
        &plan_id,
        &captcha_verify_param,
        captcha_region.as_deref(),
        device_mid,
        proxy,
    )
    .await
    .map_err(GatewayError::into_message)?;
    Ok(match outcome {
        claim::ClaimOutcome::Claimed { plan_id, starts_at, ends_at } => {
            logging::log(
                "[Claim]",
                &format!("✅ ZCode {}账号领取成功：{plan_id}", region.label()),
            );
            serde_json::json!({
                "ok": true,
                "planId": plan_id,
                "startsAt": starts_at,
                "endsAt": ends_at,
                "activated": activated,
                "activationError": activation_error,
            })
        }
        claim::ClaimOutcome::Failed { plan_id, failure, code, message, failure_ends_at } => {
            logging::log(
                "[Claim]",
                &format!(
                    "ZCode {}账号领取失败：{}（{code}）—— {message}",
                    region.label(),
                    failure.label()
                ),
            );
            serde_json::json!({
                "ok": false,
                "planId": plan_id,
                "failure": failure_key(failure),
                "failureLabel": failure.label(),
                "code": code,
                "message": message,
                "failureEndsAt": failure_ends_at,
                "activated": activated,
                "activationError": activation_error,
            })
        }
    })
}

// ─── 编码套餐通道的请求构造 ────────────────────────────────────

/// `POST {openai_base}/chat/completions`（网关 `build_chat_request` 的
/// coding-plan 分支逐字移植）。
fn coding_plan_request(
    region: Region,
    openai_base_url: &str,
    account: &Value,
    body: &Value,
) -> Result<ChatPlan, String> {
    let token = account
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if token.is_empty() {
        return Err("ZCode 账号缺少推理凭证，请重新登录".to_string());
    }
    let mut headers: Vec<(String, String)> = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "*/*".to_string()),
        ("Authorization".to_string(), format!("Bearer {token}")),
    ];
    headers.extend(identity_headers(None));
    let mut outgoing = body.clone();
    // 思考等级：这条通道没有「思考预算」这个概念，`reasoning_effort` 是唯一
    // 的旋钮，注了才拦得住「小输出额度被思考吃光、正文空串」。
    let wire_model = outgoing
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    reasoning::apply_to_chat(&mut outgoing, &wire_model);
    // 流式补 `stream_options.include_usage`：上游只在显式要求时才回用量帧，
    // 不补的话这条通道的输入/输出/缓存三列恒为 0（面板看着像没计费）。
    // **不覆盖**客户端已有的取值（那是它的选择，网关补的是缺省值）。
    ensure_include_usage(&mut outgoing);
    let url = format!("{openai_base_url}/chat/completions");
    Ok(ChatPlan::chat(
        // 端点映射：上游经 `agent/configs` 下发的 from→to 表（fail-open，
        // 没快照/没命中时原样放行，见 `endpoint_mapping` 的模块头）
        endpoint_mapping::apply(region, &url),
        headers,
        outgoing,
    ))
}

/// 流式请求补 `stream_options.include_usage = true`。
///
/// ── 为什么要补 ──────────────────────────────────────────────
/// OpenAI 协议的流式响应**默认不带用量帧**，上游只在客户端显式要求时才在流末
/// 补一帧 `usage`。不补的后果是这条通道的请求日志三列恒为 0 —— 面板看着像
/// 「这条没计费」，也让「用量对不上上游账单」这类问题失去参照物。
///
/// ── 为什么不覆盖客户端的取值 ────────────────────────────────
/// 客户端可能显式写了 `include_usage: false`（少数客户端拿它省一帧），那是它
/// 的选择；网关补的是**缺省值**，不是替它做决定。
fn ensure_include_usage(body: &mut Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    if object.get("stream").and_then(Value::as_bool) != Some(true) {
        return;
    }
    let options = object
        .entry("stream_options".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if let Some(options) = options.as_object_mut() {
        options
            .entry("include_usage".to_string())
            .or_insert_with(|| Value::Bool(true));
    }
}

// ─── 客户端身份头 ──────────────────────────────────────────────

/// 推理请求上的「客户端身份头」（参考实现 `buildLlmIdentityHeaders` 的移植）。
///
/// 编码套餐的入口是**给官方客户端用的**，上游按客户端形态识别请求。只发一个
/// 光秃秃的 `Authorization` 也能过鉴权，但上游一旦按形态限流或灰度，缺头就是
/// 难查的失败 —— 而这一套头是免费的。
///
/// 两处**故意**与参考不同（别当成漏抄）：
///   · 不发 `X-Os-Version`：参考实现取 `os.release()`，Rust 侧要为此引一个
///     系统信息 crate；它是可选头，参考实现取不到时同样省略；
///   · 不在这条路上发 `X-Device-Mid`：参考实现明确注明推理路径**从不**发它
///     （那是领取/余额那种控制面请求的要求）。
pub(super) fn identity_headers(user_agent_suffix: Option<&str>) -> Vec<(String, String)> {
    let version = claim::app_version();
    let user_agent = match user_agent_suffix.map(str::trim).filter(|text| !text.is_empty()) {
        Some(suffix) => format!("ZCode/{version} {suffix}"),
        None => format!("ZCode/{version}"),
    };
    vec![
        ("HTTP-Referer".to_string(), "https://zcode.z.ai".to_string()),
        ("User-Agent".to_string(), user_agent),
        ("X-ZCode-App-Version".to_string(), version),
        ("X-Title".to_string(), "Z Code@cli".to_string()),
        ("X-Release-Channel".to_string(), "production".to_string()),
        ("X-Client-Language".to_string(), "unknown".to_string()),
        ("X-Client-Timezone".to_string(), "unknown".to_string()),
        ("X-ZCode-Agent".to_string(), "glm".to_string()),
        ("X-Platform".to_string(), claim::platform().to_string()),
        ("X-Os-Category".to_string(), claim::os_category().to_string()),
    ]
}

// ─── 小工具 ────────────────────────────────────────────────────

/// 协议的 `headers`（`BTreeMap`）→ 本模块内部用的同名形态（键已小写）。
fn header_map(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(key, value)| (key.to_ascii_lowercase(), value.clone()))
        .collect()
}

/// 从账号记录里取账号级出口（`proxy` 字段；缺失/坏形态 = 直连）。
fn account_proxy(record: &Value) -> Option<agent2api_provider_protocol::WireProxy> {
    record
        .get("proxy")
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

/// 从账号里读访问令牌。
///
/// `account` 为空串时取该 provider 的队首账号（与网关 `current_entry_for_provider`
/// 口径一致：非空 = 用户在界面上点名的那条账号，取不到就报错而不是回落到队首 ——
/// 那会把「点名的账号坏了」变成「静默用了别人的额度」）。
async fn session_access_token(
    ctx: &PluginCtx,
    region: Region,
    account: &str,
) -> Result<String, GatewayError> {
    let record = crate::store::record(ctx, region.provider_id(), account)
        .await
        .map_err(|error| GatewayError::with_status(error.status_code, error.message))?;
    if record.is_null() {
        return Err(GatewayError::with_status(
            401,
            "没有可用的 ZCode 账号，请先在「账号」页添加",
        ));
    }
    let token = match plan_of(&record) {
        PLAN_START => crate::store::pick(&record, &["jwt"]),
        _ => crate::store::pick(&record, &["accessToken", "access_token"]),
    };
    if token.is_empty() {
        let field = if plan_of(&record) == PLAN_START { "jwt" } else { "accessToken" };
        return Err(GatewayError::with_status(
            400,
            format!("ZCode 账号的 {} 凭据字段缺失（{field}）：请检查导入记录及 pluginSecret 的存储形态；重新登录不能修复字段序列化错误", plan_label(plan_of(&record))),
        ));
    }
    Ok(token)
}

// ─── 领取结果的 JSON 形态 ──────────────────────────────────────

/// `PreviewOutcome` → 界面读的 JSON（`api::zcode_claim::preview` 的同形物）。
fn preview_json(outcome: &claim::PreviewOutcome) -> Value {
    match outcome {
        claim::PreviewOutcome::NotDeployed => serde_json::json!({
            "deployed": false,
            "plans": [],
            "message": "上游当前没有部署套餐领取接口",
        }),
        claim::PreviewOutcome::Plans(plans) => serde_json::json!({
            "deployed": true,
            "plans": plans
                .iter()
                .map(|plan| serde_json::json!({
                    "planId": plan.plan_id,
                    "name": plan.name,
                    "description": plan.description,
                    "priority": plan.priority,
                    "startsAt": plan.starts_at,
                    "endsAt": plan.ends_at,
                    "entitlements": plan.entitlements.len(),
                }))
                .collect::<Vec<_>>(),
        }),
    }
}

/// `ClaimOutcome` → 界面读的 JSON。
fn claim_json(outcome: &claim::ClaimOutcome) -> Value {
    match outcome {
        claim::ClaimOutcome::Claimed {
            plan_id,
            starts_at,
            ends_at,
        } => serde_json::json!({
            "claimed": true,
            "planId": plan_id,
            "startsAt": starts_at,
            "endsAt": ends_at,
        }),
        claim::ClaimOutcome::Failed {
            plan_id,
            failure,
            code,
            message,
            failure_ends_at,
        } => serde_json::json!({
            "claimed": false,
            "planId": plan_id,
            "failure": format!("{failure:?}"),
            "code": code,
            "message": message,
            "failureEndsAt": failure_ends_at,
        }),
    }
}

// ─── 账号视图 / 身份 / 规范化 ──────────────────────────────────

/// 账号公开视图（界面账号页读的那几个键）。
fn account_view(region: Region, secret: &Value) -> Value {
    let access_token = crate::store::pick(secret, &["accessToken", "access_token"]);
    let jwt = crate::store::pick(secret, &["jwt"]);
    let user_id = crate::store::pick(secret, &["userId", "user_id"]);
    let email = crate::store::pick(secret, &["email"]);
    let name = {
        let explicit = crate::store::pick(secret, &["name"]);
        if !explicit.is_empty() {
            explicit
        } else if !email.is_empty() {
            email
        } else if !user_id.is_empty() {
            format!("ZCode {} · {user_id}", region.label())
        } else {
            format!("ZCode {}", region.label())
        }
    };
    let token = if !access_token.is_empty() {
        access_token
    } else {
        jwt.clone()
    };
    let mut view = serde_json::json!({
        "name": name,
        "userId": user_id,
        "plan": plan_of(secret),
        "planLabel": plan_label(plan_of(secret)),
        "tokenTail": crate::store::token_tail(&token),
        "hasSession": !token.is_empty(),
        // 上游没有续期协议 —— 界面据此不渲染「刷新」按钮
        "canRefresh": false,
        "canClaim": !jwt.is_empty(),
    });
    if let Some(expires_at) = credentials::expires_at_ms(&credentials::ZcodeCredentials {
        region,
        access_token: crate::store::pick(secret, &["accessToken", "access_token"]),
        jwt: crate::store::pick(secret, &["jwt"]),
        user_id: String::new(),
        device_mid: String::new(),
    }) {
        if let Some(object) = view.as_object_mut() {
            object.insert("expiresAt".to_string(), crate::json::json_number(expires_at));
        }
    }
    view
}

/// 账号的稳定身份（去重用）：`<地区前缀><user_id>`。
///
/// 与网关 `ZcodeCredentials::account_id` 同一口径 —— 两地的记录天然不相交
/// （同一个人在两套系统里的 user_id 可能相同）。
fn identity_of(secret: &Value) -> String {
    let user_id = crate::store::pick(secret, &["userId", "user_id"]);
    if user_id.is_empty() {
        return String::new();
    }
    // 地区前缀由调用方（`normalize`）拼在 secret 里，见下
    let prefix = crate::store::pick(secret, &["idPrefix"]);
    format!("{prefix}{user_id}")
}

/// 粘贴凭证的三种形态（网关 `api::accounts` 的 zcode 分支逐条对齐）。
async fn normalize(
    region: Region,
    input: proto::NormalizeInput,
) -> Result<proto::NormalizedAccount, GatewayError> {
    let payload = match input.source {
        proto::NormalizeSource::Manual { payload } => payload,
        proto::NormalizeSource::FileText { content, .. } => {
            serde_json::from_str::<Value>(content.trim()).map_err(|_| {
                GatewayError::with_status(
                    400,
                    "不是合法 JSON（请粘贴含 accessToken / jwt 的凭证对象）",
                )
            })?
        }
        proto::NormalizeSource::FileBase64 { .. } => {
            return Err(GatewayError::with_status(
                400,
                "ZCode 不支持二进制凭证文件，请粘贴访问令牌或使用「网页登录」",
            ))
        }
        proto::NormalizeSource::Desktop => {
            return Err(GatewayError::with_status(
                400,
                "ZCode 不支持导入本机登录态（CLI 登录态在网关侧完成），\
                 请用「网页登录」或粘贴访问令牌",
            ))
        }
    };
    let access_token = crate::store::pick(
        &payload,
        &["accessToken", "access_token", "apiKey", "api_key"],
    );
    let jwt = crate::store::pick(&payload, &["jwt", "token"]);
    if access_token.is_empty() && jwt.is_empty() {
        return Err(GatewayError::with_status(
            400,
            "请至少提供一项凭证：编码套餐的 accessToken，或套餐池的 jwt",
        ));
    }
    // 设备标识：用户给了就用（换设备号 = 换设备身份，是用户的选择），
    // 没给就现生成一个并随账号落盘（见 `balance` 的说明）
    let explicit_device = crate::store::pick(&payload, &["deviceMid", "device_mid"]);
    let device_mid = if explicit_device.is_empty() {
        credentials::new_device_mid().unwrap_or_default()
    } else {
        explicit_device
    };
    let user_id = crate::store::pick(&payload, &["userId", "user_id"]);
    let plan = payload
        .get(PLAN_FIELD)
        .and_then(Value::as_str)
        .and_then(normalize_plan)
        .unwrap_or(PLAN_CODING);
    let mut secret = serde_json::json!({
        "accessToken": access_token,
        "jwt": jwt,
        "deviceMid": device_mid,
        "userId": user_id,
        "zcodePlan": plan,
        // 去重前缀（`identity_of` 读它）—— 与网关 account_id 的前缀同值
        "idPrefix": region.account_id_prefix(),
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
        identity: identity_of(&secret),
        secret,
        desktop: false,
    })
}
