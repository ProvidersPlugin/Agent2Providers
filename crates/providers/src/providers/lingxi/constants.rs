//! WPS 灵犀的协议常量与内置模型表（2026-10-28 一手实测）。
//!
//! ── 两条完全不同的平面（别混）───────────────────────────────
//!   1. **cowork / 会话平面**：`/api/aigc/v3/assistant/sessions/{id}/completions`，
//!      上行只有 `user_input.text` 一段文本，**没有 tools 槽位**（magpie 插件与
//!      我们 10-16 的观测都指向这一点）；
//!   2. **agent-runtime 直连平面**：`/api/public/v1/agent-runtime/chat/completions`，
//!      OpenAI 风格（`messages` / `tools` / `tool_calls`），**工具真可用**。
//!      本 provider 用的是这一条（实测：finish_reason=tool_calls、参数完整、
//!      工具结果回灌后自然收尾、`stream:false` 也通）。
//!
//! ── 三步调用链（缺一不可）───────────────────────────────────
//! ```text
//!   ① POST /api/aioffice/v1/cowork/sessions   → session_id
//!   ② POST /api/public/v1/credits/runs        → run_id（会话语义，可复用）
//!   ③ POST /api/public/v1/agent-runtime/chat/completions（SSE）
//! ```
//! `chat/completions` 的 `x-cc-session-id` / `x-cc-run-id` 必须来自真实创建的
//! 对象（假值报 `SQLNoRows`）。计费按 **run**：一个 run 内多次调用只扣一次智点，
//! 所以本实现按「账号 × 会话 × 模型」缓存 run 并复用。
//!
//! ── 鉴权 ────────────────────────────────────────────────────
//! 只有 `Cookie: wps_sid=<sid>`（网页登录态，约一年有效），不认 Bearer。

/// provider id（`pack.json` 里必须同一个）。
pub const ID: &str = "lingxi";

/// 云端基址（注意是 `lingxi.wps.cn`，不是 magpie 用的 `lingxi.kdocs.cn`）。
pub const BASE: &str = "https://lingxi.wps.cn";

/// 三步链路 + 目录 / 余额 / 任务。
pub const SESSIONS_PATH: &str = "/api/aioffice/v1/cowork/sessions";
pub const RUNS_PATH: &str = "/api/public/v1/credits/runs";
pub const CHAT_PATH: &str = "/api/public/v1/agent-runtime/chat/completions";
pub const PLANS_PATH: &str = "/api/aioffice/v1/sessions/plans";
pub const BALANCE_PATH: &str = "/api/public/v1/credits/balance";
pub const TASKS_PATH: &str = "/api/public/v1/tasks";
pub const CLAIM_PATH: &str = "/api/public/v1/tasks/daily_check_in/claim";

/// 客户端身份头（真机桌面版的两个；服务端目前不严格校验，但保持一致）。
pub const CLIENT_TYPE: &str = "lingxi_claw";
pub const APPSERVER_VERSION: &str = "68";

/// 对外模型 id 前缀：`GLM-5.3` 在 codearts / loomy 等同名，必须隔离。
pub const MODEL_PREFIX: &str = "lingxi/";

/// 缓存 run 的存活时间（与参考实现同量级；过期后重建一个 run）。
pub const RUN_TTL_MS: i64 = 60 * 60 * 1000;

/// 一条内置模型（远程目录拉到后以远程为准）。
pub struct FallbackModel {
    pub key: &'static str,
    pub name: &'static str,
    pub multiplier: &'static str,
    pub vision: bool,
    pub max_input: i64,
    pub max_output: i64,
    pub reasoning: bool,
}

/// 2026-10-28 实测的 10 个模型（`/api/aioffice/v1/sessions/plans`）。
pub const FALLBACK_MODELS: &[FallbackModel] = &[
    FallbackModel { key: "deepseek-v4.1-flash", name: "DeepSeek V4.1 Flash", multiplier: "～0.12x", vision: true, max_input: 1_048_576, max_output: 393_216, reasoning: true },
    FallbackModel { key: "deepseek-v4-pro-0813", name: "DeepSeek V4 Pro", multiplier: "0.40x", vision: false, max_input: 1_048_576, max_output: 393_216, reasoning: true },
    FallbackModel { key: "qwen3.8_flash", name: "Qwen3.8-Flash", multiplier: "～0.08x", vision: true, max_input: 1_048_576, max_output: 131_072, reasoning: true },
    FallbackModel { key: "qwen3.8_max", name: "Qwen3.8-Max", multiplier: "～0.80x", vision: true, max_input: 1_048_576, max_output: 131_072, reasoning: true },
    FallbackModel { key: "mimo-v2.6-pro", name: "Xiaomi MiMo-V2.6 Pro", multiplier: "0.10x", vision: true, max_input: 1_048_576, max_output: 131_072, reasoning: false },
    FallbackModel { key: "GLM-5.3-Flash", name: "GLM-5.3-Flash", multiplier: "~0.11x", vision: true, max_input: 1_048_576, max_output: 131_072, reasoning: true },
    FallbackModel { key: "GLM-5.3", name: "GLM-5.3", multiplier: "~0.75x", vision: false, max_input: 1_048_576, max_output: 131_072, reasoning: true },
    FallbackModel { key: "minimax_m3", name: "MiniMax-M3", multiplier: "0.20x", vision: true, max_input: 1_048_576, max_output: 524_288, reasoning: false },
    FallbackModel { key: "doubao_seed_2_1_pro_260628", name: "Doubao-Seed-2.1 Pro", multiplier: "～0.50x", vision: true, max_input: 262_144, max_output: 262_144, reasoning: true },
    FallbackModel { key: "kimi-k3", name: "Kimi-K3", multiplier: "1.60x", vision: true, max_input: 1_048_576, max_output: 1_048_576, reasoning: true },
];

/// 内置默认模型（请求未指定 / 客户端只说 `auto` 时的落点）。
pub const DEFAULT_MODEL: &str = "GLM-5.3";

/// 模型 id（对外）→ 上游 key。
pub fn wire_key(model_id: &str) -> String {
    let trimmed = model_id.trim();
    let bare = trimmed.strip_prefix(MODEL_PREFIX).unwrap_or(trimmed);
    if bare.is_empty() {
        DEFAULT_MODEL.to_string()
    } else {
        bare.to_string()
    }
}
