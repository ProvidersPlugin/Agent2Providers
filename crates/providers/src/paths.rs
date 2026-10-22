//! 插件自己的落盘目录（网关 `config::config_dir()` 的插件侧等价物）。
//!
//! ── 为什么用 `HostEnv.data_dir` 而不是自己找 ──────────────────
//! 网关在握手时把数据目录交给插件（`HostEnv.data_dir`，见协议 crate）。那是
//! 用户可见的那个目录（配置文件、账号库都在里面），插件在里面建自己的子目录
//! 就能与网关的备份 / 迁移一起走。
//!
//! ── 为什么是进程级 OnceLock 而不是每次从 ctx 读 ────────────────
//! 搬来的代码（Qoder 的机器标识、UMID 组件缓存）是**同步函数**里的路径拼接，
//! 拿不到 `&PluginCtx`。进程级缓存一份目录是唯一可行的形态 —— 目录在进程
//! 生命周期内不变（网关只在 `init` 时给一次）。
//!
//! ── 没初始化时返回什么 ──────────────────────────────────────
//! 退回系统临时目录下的固定子目录（而不是空路径）：搬来的代码会 `create_dir_all`
//! 并写文件，空路径会写到当前工作目录。退回到临时目录至少是「能跑，但重启
//! 后丢」—— 而且这个分支只在 `init` 之前被调到，正常流程里不会发生。

use std::path::PathBuf;
use std::sync::OnceLock;

static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// 记录网关给的数据目录（`main` 在启动时调一次）。
///
/// ── 当前没有调用点（等 SDK 的 init 回调）────────────────────
/// 见 `main.rs` 的 `_document_init_gap`：`serve_blocking` 不暴露「init 完成」
/// 的钩子，所以这条路径暂时走不到，`data_dir()` 退回临时目录。
#[allow(dead_code)]
pub fn init(data_dir: &str) {
    let trimmed = data_dir.trim();
    if trimmed.is_empty() {
        return;
    }
    // 已初始化过就不覆盖：同一进程只会有一个网关
    let _ = DATA_DIR.set(PathBuf::from(trimmed));
}

/// 插件的数据目录。
pub fn data_dir() -> PathBuf {
    match DATA_DIR.get() {
        Some(dir) => dir.clone(),
        None => std::env::temp_dir().join("agent2api-providers"),
    }
}
