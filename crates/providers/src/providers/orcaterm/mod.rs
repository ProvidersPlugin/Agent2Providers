//! OrcaTerm（腾讯云遨驰终端桌面客户端）插件实现。
//!
//! ── 上游长什么样（2026-10-16 桌面客户端 1.6.4 + Web 版实测）──────────
//! ```text
//! POST {LIGHTAI}/assistant/conversation                       建会话
//! POST {LIGHTAI}/assistant/chat?name=orcaterm&mode=0&…        SSE 对话
//! POST {CGI}/cgi/api?action=GetUserInfo&uin=<uid>             账号信息
//! POST {CGI}/cgi/api?action=DescribeUserBillingStatus&uin=…   月度配额
//! POST {CGI}/cgi/api?action=OAuthRefreshToken&uin=…           续期
//! POST {CGI}/cgi/api?action=OAuthExchangeToken                网页登录换码
//! ```
//!
//! ── 与网关内置版的差别（只有一处是实质性的）─────────────────
//! 凭证不再从 `AccountStore` 的强类型方法取，而是从 `ctx.store_get` 拿到的
//! **不透明 JSON**（`secret`）里读 —— 形状由本模块自己定义、自己解释。
//! 其余逻辑（建会话 / SSE 翻译 / 信封解包 / 401 自愈 / 配额归一）逐字沿用。
//!
//! ── 有状态：一次转发 = 建会话 → SSE 两步 ─────────────────────
//! `is_stateful()` 为 true 的含义是「一次转发要由适配器自己完成多步时序」，
//! **不是**「跨请求保有上游状态」：会话是请求内的（每个请求新建、用毕即弃），
//! 多轮历史拍平进输入文本。因此没有会话注册表、无指纹链、重启即忘。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 本 crate 编译期禁止 unwrap/expect/panic（见 `main.rs` 的 deny）。

pub mod balance;
pub mod chat;
pub mod credentials;
pub mod http;
pub mod login;
pub mod models;

use std::collections::BTreeMap;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{
    ConversationSink, PluginCtx, Provider, ProviderMeta, SendFuture,
};
use serde_json::{json, Value};

/// provider id（落盘契约，不能改）。
pub const ID: &str = "orcaterm";

/// CGI 网关（账号 / 配额 / 续期）。
pub const CGI_BASE: &str = "https://api.orcaterm.cloud.tencent.com";

/// AI 助手网关（对话 + 会话管理）。
pub const LIGHTAI_BASE: &str = "https://lightai.cloud.tencent.com";

/// 桌面端登录态文件（Tauri store，明文 JSON）。
pub const DESKTOP_DATA_FILE: &str = "com.orcaterm-desktop.app/data.bin";

/// 出站 User-Agent（Web 版同款；上游对默认 UA 敏感）。
pub const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 ",
    "(KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36"
);

/// assistant 接口的固定 query。
pub const ASSISTANT_QUERY: &str = "name=orcaterm&mode=0&csrfCode=&sseresume=true";

/// 默认模型（未知模型显式回退到实测可用的这一档）。
pub const DEFAULT_MODEL: &str = "TokenHub/glm-5.3-flash";

/// 凭证在账号 `secret` 里的键名。
///
/// ── 为什么用自己的键名而不是 `accessToken` ────────────────────
/// `secret` 是**本插件的不透明 JSON**（网关不解析）。用 `accessToken` 会让
/// 它与内置家的字段名撞车 —— 将来若有工具按那个名字跨家读取，就会读到
/// 一个语义不同、过期规则也不同的值。加前缀是零成本的隔离。
pub const SECRET_TOKEN: &str = "accessToken";
pub const SECRET_EXPIRES_AT: &str = "tokenExpireAt";
pub const SECRET_UID: &str = "uid";
pub const SECRET_NAME: &str = "loginName";

pub struct OrcaTermProvider;

impl Provider for OrcaTermProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(ID, "OrcaTerm")
            .with_capabilities(proto::Capabilities {
                chat: true,
                // 一次转发 = 建会话 + SSE 两步（见模块头）
                stateful: true,
                refresh: true,
                model_refresh: false,
                usage: true,
                checkin: false,
                claims: false,
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
                    label: "accessToken".into(),
                    placeholder: "粘贴 OrcaTerm 的 JWT，或整个 data.bin 的内容".into(),
                    extra_fields: vec![],
                },
                proto::LoginMethod::Browser {
                    button: "微信扫码登录".into(),
                    hint: Some("打开 OrcaTerm 授权页，扫码后自动回填账号".into()),
                },
                proto::LoginMethod::Desktop {
                    button: "导入本机登录态".into(),
                    hint: Some("读取 OrcaTerm 桌面客户端已登录的账号".into()),
                },
            ])
            .with_models(models::list())
            // 账号行：`usage` —— 有余额概念（`DescribeUserBillingStatus` 的月度
            // AI 配额，归一成 `available` + `unit: "次"`）。
            //   `identifier: uid` —— 公开形态给的是 `uid`。
            //   `expiry: tokenExpireAt` —— ⚠ 与别家都不同的字段名（不是
            //     `tokenExpiresAt`）：公开形态里就叫 `tokenExpireAt`，写错只会让
            //     有效期列一直显示「—」。
            //   `edition: false` —— 单一入口（桌面端登录态 / 粘贴令牌）。
            //   没有 `credit_detail`：`query_usage` 只给 `available` 与
            //     subscription，不出逐份 `credits[]`，挂入口会点开一个空框。
            .with_account_features(proto::AccountFeatures {
                usage: true,
                identifier: vec!["uid".to_string()],
                expiry: vec!["tokenExpireAt".to_string()],
                ..Default::default()
            })
    }

    // ── 数据面 ────────────────────────────────────────────────

    fn conversation<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ConversationRequest,
        sink: ConversationSink<'a>,
    ) -> SendFuture<'a, ()> {
        Box::pin(chat::run(ctx, req, sink))
    }

    /// 一律 Fatal：上游没有 HTTP 429（限额错误在流内 `LLM_ERROR` 帧里），
    /// 且配额按月重置 —— 做冷却记账的收益为负（冷却键是账号级配额，
    /// 误冷却一整个月比透传错误更糟）。
    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        let message = body
            .get("message")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误")
            .to_string();
        proto::ErrorClass::Fatal { status, message, upstream_code: None }
    }

    // ── 凭证 ──────────────────────────────────────────────────

    fn ensure_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let mut credentials = credentials::snapshot(ctx, account).await?;
            credentials = credentials::refresh(ctx, &credentials, account, false).await?;
            Ok(credentials.access_token)
        })
    }

    fn refresh_token<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, String> {
        Box::pin(async move {
            let credentials = credentials::snapshot(ctx, account).await?;
            let next = credentials::refresh(ctx, &credentials, account, true).await?;
            Ok(next.access_token)
        })
    }

    fn token_expiring<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, bool> {
        Box::pin(async move {
            // 判不出来就 false：宁可漏刷，也不要凭猜测打上游
            let Ok(credentials) = credentials::snapshot(ctx, account).await else {
                return Ok(false);
            };
            Ok(credentials.is_expiring())
        })
    }

    // ── 目录 / 余额 ───────────────────────────────────────────

    fn models<'a>(&'a self, _ctx: &'a PluginCtx, _account: &'a str) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async {
            Ok(proto::ModelList {
                models: models::list(),
                meta: proto::CatalogMeta { remote: false, refreshed_at: 0 },
            })
        })
    }

    fn usage<'a>(&'a self, ctx: &'a PluginCtx, account: &'a str) -> SendFuture<'a, Value> {
        Box::pin(balance::query_usage(ctx, account))
    }

    // ── 账号 ──────────────────────────────────────────────────

    fn normalize_account<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(credentials::normalize(ctx, input))
    }

    fn account_view(&self, secret: &Value) -> Result<Value, String> {
        credentials::account_view(secret)
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        Ok(credentials::identity_of(secret))
    }

    // ── 登录 ──────────────────────────────────────────────────

    fn login_start<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        method: &'a str,
        payload: &'a Value,
    ) -> SendFuture<'a, proto::LoginStart> {
        Box::pin(async move { login::start(method, payload) })
    }

    fn login_callback<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        callback: proto::LoginCallback,
    ) -> SendFuture<'a, proto::LoginPoll> {
        Box::pin(login::callback(ctx, callback))
    }
}

/// 把一组头转成协议里的 `BTreeMap`（小写键）。
///
/// 各家的 `credentials::*_headers` 返回 `Vec<(String, String)>`（顺序有意义：
/// 后写的覆盖先写的），而 `StreamHead.headers` 是 map —— 转一次即可。
pub(crate) fn header_map(headers: &[(String, String)]) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(key, value)| (key.to_ascii_lowercase(), value.clone()))
        .collect()
}

/// 拼一个账号公开视图（前端账号页用）。
pub(crate) fn public_view(name: &str, uid: &str, tail: &str) -> Value {
    json!({
        "name": name,
        "uid": uid,
        "tokenTail": tail,
    })
}
