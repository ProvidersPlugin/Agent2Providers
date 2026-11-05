//! AStudio（科大讯飞 Astron Studio 桌面端）的地址与常量。
//!
//! ── 依据 ────────────────────────────────────────────────────
//! 参考实现：magpie 社区 `@magpie-community/opencode-astudio-auth`
//! （`Acankao/magpie-plugins/packages/astudio`，MIT）。
//!
//! ```text
//!   模型调用  POST https://astronstudio-api-volces-prod.xf-yun.com/model-gateway/v1/responses
//!             （OpenAI Responses 协议，Authorization: Bearer <modelBearerToken>）
//!   模型目录  GET  https://astronstudio-api-volces-prod.xf-yun.com/api/v1/model-manager/models
//!   账号面    https://agent.xfyun.cn/xingchen-studio/…
//!             points/balance、membership/me、membership/plan-config
//!   每日登录  POST tenant-app/v2/init-app（带网页会话，无请求体）
//! ```
//!
//! 这些地址来自桌面端写进 acode 内核的 `base_url` / `wire_api`，与官方客户端
//! 完全一致。

/// provider id（`pack.json` 里也要写同一个）。
pub const PROVIDER_ID: &str = "astudio";

/// Responses 端点。
pub const RESPONSES_URL: &str =
    "https://astronstudio-api-volces-prod.xf-yun.com/model-gateway/v1/responses";

/// 模型目录。
pub const MODEL_MANAGER_URL: &str =
    "https://astronstudio-api-volces-prod.xf-yun.com/api/v1/model-manager/models";

/// 账号面根（注意结尾的斜杠：相对路径拼在它后面）。
pub const STUDIO_API_BASE: &str = "https://agent.xfyun.cn/xingchen-studio/";

/// Windows 注册表：桌面端把数据根目录登记在这里。
pub const REGISTRY_KEY: &str = "HKCU\\Software\\AStudioStorage";

/// 注册表里的三种频道。
pub const REGISTRY_PREFIXES: &[&str] = &["Production", "Canary", "Development"];

/// 账号面接口的相对路径。
pub const POINTS_BALANCE: &str = "points/balance";
pub const MEMBERSHIP_ME: &str = "membership/me";
pub const PLAN_CONFIG: &str = "membership/plan-config";

/// 当日首次调用触发每日登录奖励（POST，无请求体）；登录与签到共用。
pub const INIT_APP: &str = "tenant-app/v2/init-app";
