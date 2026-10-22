//! OrcaTerm 模型目录：静态兜底（上游没有可拉的清单接口）。
//!
//! ── 为什么是静态表 ──────────────────────────────────────────
//! 模型清单来自**客户端内置**（桌面端 1.6.4 的模型下拉面板，2026-10-16
//! 对照截图全量核对；Web 版面板的 `DescribeAIModelList` 对普通账号返回空
//! 数组，不是清单来源）。上游没有「模型清单」的 HTTP 接口 —— 面板打开时的
//! 全部请求都不携带模型列表，下拉项与桌面端前端打包内置一致。
//!
//! ── 与面板的两处有意差异 ────────────────────────────────────
//! `hy4-preview` 面板有、上游未挂载（真发对话回流内
//! `LLM_ERROR: LLM(TokenHub/hy4-preview) is not loaded`）。仍然收录 ——
//! 面板是用户可见的事实，不收录会让「面板里那个模型在网关里消失」；
//! 选中它时的明确错误由 `chat.rs` 的流内错误下发兜底（比静默回落更诚实）。

use serde_json::{json, Value};

use super::DEFAULT_MODEL;

/// 静态清单（与桌面端面板逐项对齐；`(id, 面板展示名)`，顺序 = 面板顺序）。
const MODELS: [(&str, &str); 9] = [
    ("hy4-preview", "Hy4-preview"),
    ("hy3", "Hy3"),
    ("kimi-k3", "Kimi-K3"),
    ("glm-5.3-flash", "GLM-5.3-Flash"),
    ("glm-5.3", "GLM-5.3"),
    ("glm-5.2", "GLM-5.2"),
    ("deepseek-v4.1-flash", "DeepSeek-V4.1-Flash"),
    ("deepseek-v4-flash", "DeepSeek-V4-Flash"),
    ("deepseek-v4-pro", "DeepSeek-V4-Pro"),
];

/// 模型清单（聚合层 `models::list_item` 的口径：id + name）。
pub fn list() -> Vec<Value> {
    MODELS
        .iter()
        .map(|(id, name)| json!({ "id": id, "name": name }))
        .collect()
}

/// 把客户端请求的模型名归一成上游的 `TokenHub/<name>`。
///
/// 接受裸名与全名两种形态；未知模型**显式回退**默认档（上游对未知名回流内
/// `LLM_ERROR`，与其让它失败不如回退到实测可用的那一档）。清单内的模型
/// （含未挂载的 `hy4-preview`）原样放行 —— 选中后的上游错误由 `chat.rs`
/// 明确下发，不做静默替换。
pub fn resolve_model(name: &str) -> String {
    let name = name.trim().trim_start_matches("TokenHub/").trim();
    if name.is_empty() {
        return DEFAULT_MODEL.to_string();
    }
    if MODELS.iter().any(|(id, _)| *id == name) {
        format!("TokenHub/{name}")
    } else {
        DEFAULT_MODEL.to_string()
    }
}
