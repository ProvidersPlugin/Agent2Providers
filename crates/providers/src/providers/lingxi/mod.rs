//! WPS 灵犀（Lingxi Claw）provider。
//!
//! ── 接的是哪条平面（2026-10-28 一手实测）─────────────────────
//! `POST https://lingxi.wps.cn/api/public/v1/agent-runtime/chat/completions`：
//! OpenAI 风格（`messages` / `tools` / `tool_calls`），**工具真可用**
//! （finish_reason=tool_calls、参数完整、结果回灌后自然收尾、非流式也通）。
//!
//! 注意与另一条 `cowork / assistant sessions` 平面区分：那条上行只有
//! `user_input.text` 一段文本、没有 tools 槽位（magpie 插件与我们的旧观测
//! 都指向它），不要拿它来接 agent 客户端。
//!
//! ── 三步链路 ────────────────────────────────────────────────
//! ① 建 cowork session ② 建 credits run ③ 调 agent-runtime（头带
//! `x-cc-session-id` / `x-cc-run-id`）。run 是计费单位，按会话缓存复用。
//!
//! ── 鉴权 ────────────────────────────────────────────────────
//! 只认 `Cookie: wps_sid=…`（网页登录态，约一年），没有 refresh 接口；
//! 失效时只能重新粘贴 Cookie —— 所以 `refresh = false`。
//!
//! ── 能力位 ──────────────────────────────────────────────────
//! 对话（有状态）/ 目录 / 余额 / 每日签到；没有「领取类」独立入口，
//! 没有桌面端导入（Windows 的 DPAPI 密文插件读不到）。

pub mod account;
pub mod billing;
pub mod catalog;
pub mod chat;
pub mod constants;
pub mod http;
pub mod stream;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::{
    ConversationSink, PluginCtx, Provider, ProviderMeta, SendFuture,
};
use serde_json::Value;

/// provider 实例（`registry()` 里装箱）。
pub struct LingxiProvider;

impl Provider for LingxiProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new(constants::ID, "WPS 灵犀")
            .with_capabilities(proto::Capabilities {
                chat: true,
                // 三步链路（session / run / 直连口）+ 自定义 SSE 翻译
                stateful: true,
                // 只有 Cookie，没有 refresh 接口
                refresh: false,
                model_refresh: true,
                usage: true,
                checkin: true,
                claims: false,
                // 网页登录要靠壳侧读登录窗口的 Cookie，插件没有这条通道
                web_login: false,
                // 帧里的 model 是上游 key（GLM-5.3），要回写成请求名（lingxi/GLM-5.3）
                sse_model_rewrite: true,
                sse_strip_newline_chunks: false,
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                refresh_uses_account: false,
                supports_default_model: false,
                env_credentials: false,
            })
            .with_login(vec![proto::LoginMethod::Token {
                label: "WPS 会话 Cookie".to_string(),
                placeholder: "粘贴 wps_sid 的值，或整段 Cookie（含 wps_sid=…）".to_string(),
                extra_fields: Vec::new(),
            }])
            .with_models(catalog::fallback())
            .with_account_features(proto::AccountFeatures {
                usage: true,
                credit_detail: true,
                credit_expiry: true,
                usage_detail: false,
                edition: false,
                identifier: vec!["uid".to_string(), "name".to_string()],
                // wps_sid 是登录态 Cookie，没有对外可读的到期时刻
                expiry: Vec::new(),
                long_lived: false,
                ..Default::default()
            })
            .with_checkin(proto::CheckinInfo {
                description: Some("每日签到领智点（默认 100，连续第 7 天 200）".to_string()),
                exclude_guest: false,
                modes: vec![proto::CheckinMode::Checkin],
                onboarding: false,
                plans: false,
                daily: true,
                growth: false,
                ui_hooks: Vec::new(),
            })
    }

    fn conversation<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ConversationRequest,
        sink: ConversationSink<'a>,
    ) -> SendFuture<'a, ()> {
        Box::pin(chat::conversation(ctx, req, sink))
    }

    fn models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async move { Ok(catalog::list(ctx, account, None).await) })
    }

    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move { Ok(catalog::refresh(ctx, account, None, force).await) })
    }

    fn usage<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::UsageReport> {
        Box::pin(billing::usage(ctx, account, None))
    }

    fn checkin<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::CheckinReport> {
        Box::pin(billing::checkin(ctx, account, None))
    }

    fn ensure_token<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, String> {
        Box::pin(account::sid_for(ctx, account))
    }

    fn normalize_account<'a>(
        &'a self,
        _ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move { account::normalize(&input) })
    }

    fn account_view(&self, secret: &Value) -> Result<proto::AccountView, String> {
        account::view(secret)
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        let sid = secret.get("sid").and_then(Value::as_str).unwrap_or("");
        if sid.is_empty() {
            return Ok(String::new());
        }
        Ok(account::identity_of(sid))
    }

    /// 上游错误分类。Cookie 失效没有刷新接口，`TokenExpired` 只会白试一次，
    /// 所以 401/403 直接按 Fatal 交回（文案指路重新粘贴）。
    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        let message = body
            .get("message")
            .or_else(|| body.get("error").and_then(|error| error.get("message")))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("灵犀上游返回 {status}"));
        match status {
            401 | 403 => proto::ErrorClass::Fatal {
                status,
                message: format!("{message}（WPS 登录态可能已失效，请重新粘贴 Cookie）"),
                upstream_code: None,
            },
            429 => proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: None,
                status,
            },
            _ => proto::ErrorClass::Fatal { status, message, upstream_code: None },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_declares_the_expected_capabilities() {
        let meta = LingxiProvider.meta();
        assert_eq!(meta.id, "lingxi");
        assert!(meta.capabilities.chat && meta.capabilities.stateful);
        assert!(meta.capabilities.usage && meta.capabilities.checkin);
        assert!(!meta.capabilities.refresh, "只有 Cookie，没有刷新接口");
        assert!(meta.capabilities.model_refresh);
        assert_eq!(meta.default_models.len(), 10);
        assert_eq!(meta.login_methods.len(), 1);
    }

    #[test]
    fn auth_failures_are_fatal_not_token_expired() {
        let class = LingxiProvider.classify(401, &serde_json::json!({ "message": "unauthorized" }));
        assert!(matches!(class, proto::ErrorClass::Fatal { .. }));
    }
}
