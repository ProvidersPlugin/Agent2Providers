use agent2api_wasm_sdk::{json, Error, Value};

pub const SUMMARY: &str = "CodeArts 已接通对话与会话心跳、网页登录、完整材料自动续期、模型目录、余额、每日福利和新手礼。";
pub const REFRESH: &str = "当账号包含完整 refresh_token、PKCE verifier 与 DPoP 密钥材料时，由插件在临期或强制刷新时单次轮换并 CAS 写回；无论网页登录或凭据 JSON 导入均支持自动续期。";
pub const HTTP_BUDGET: &str = "余额和福利请求总期限 20 秒，目录 30 秒；每次请求同时受原调用总预算约束。";
pub const IDENTITY: &str = "CodeArts 账号需要完整 domain_id 和 user_id；缺失时不按 AK 尾号合并账号。";
pub const IMPORT: &str = "导入或粘贴完整凭据 JSON 或使用官方网页登录；保留 OAuth 长期材料即可全平台自动续期。";

pub fn chat() -> Error { Error::http(400,"CodeArts 当前通道仅提供模型对话，不提供独立图片生成或编辑接口").with_code("unsupported_parameter") }
pub fn claim() -> Error { Error::http(400,"CodeArts 福利请从每日签到或新手礼入口领取；不购买套餐或代做认证任务").with_code("unsupported") }
pub fn status() -> Value {
    json!({"provider":"codearts","stage":"complete","complete":true,"verified":false,"message":SUMMARY,
        "implemented":["manual_credentials","account_identity","model_catalog","subscription_usage","conversation","session_heartbeat_cleanup",
            "pkce_dpop_login","ticket_fallback","oauth_refresh","welfare_claim_confirm","onboarding_claim"],
        "pending":[],"httpBudget":HTTP_BUDGET,"refresh":REFRESH,"credentialImport":IMPORT,
        "conversation":{"concurrencyPerAccount":3,"singleSubmission":true,"heartbeatSeconds":30,"cancelCleanup":true},
        "refreshForms":{"oauth":"本次原刷新令牌、PKCE verifier 和 DPoP 私钥","temporaryWithoutRefresh":"临时兜底登录无法自动续期，到期需重新登录","unknownOutcome":"不重发旧一次性材料"},
        "verification":"real_account_not_verified"})
}
