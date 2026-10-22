//! 本进程**正在驱动**的上游任务表（task id 集合）。
//!
//! ── 为什么需要它 ────────────────────────────────────────────
//! 建任务撞 10811 时会做一次自愈：列出账号上运行中的任务、逐个停止、重试
//! （见 `chat.rs::create_task_with_recovery`）。但那份列表是**账号级**的 ——
//! 同一个账号上可能还有本进程**别的请求**正在跑的任务。不排除它们就会把
//! 别人正在用的任务停掉：那个请求的 WS 会立刻断，用户看到的是「任务流连接
//! 已关闭」。
//!
//! 因此这里维护一份「本进程正在驱动」的 task id 集合：建任务成功后登记，
//! 收尾（正常 / 出错 / 断开 / 停止）时注销；自愈时**跳过集合里的 id**，
//! 只清理真正的残留。
//!
//! ── 边界（如实说明）────────────────────────────────────────
//!   - 这是**进程内**的视图：网关重启后集合清空，重启前留下的任务会被当成
//!     残留清理掉 —— 那正是我们想要的（那些任务已经没有人在读它们的流了）；
//!   - 多进程 / 多实例共用同一账号时，看不到对方的在途任务；这种情况下自愈
//!     可能停掉另一个实例正在用的任务。当前部署形态（单机单进程网关）不受影响。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic；锁不跨 await。

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// 进程内全局表。`OnceLock` + `Mutex`：只在建任务 / 收尾两个时刻短暂持锁。
static IN_FLIGHT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn table() -> &'static Mutex<HashSet<String>> {
    IN_FLIGHT.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 取锁（中毒时取回内部数据 —— 表本身只是一组 id，没有需要维护的不变量）
fn lock_table() -> std::sync::MutexGuard<'static, HashSet<String>> {
    table()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 登记一个本进程正在驱动的任务（建任务成功后立刻调用）
pub fn register(task_id: &str) {
    let id = task_id.trim();
    if id.is_empty() {
        return;
    }
    lock_table().insert(id.to_string());
}

/// 注销（收尾时调用；重复注销是幂等的）
pub fn unregister(task_id: &str) {
    let id = task_id.trim();
    if id.is_empty() {
        return;
    }
    lock_table().remove(id);
}

/// 这个任务是不是本进程正在驱动的（自愈时据此跳过）
pub fn contains(task_id: &str) -> bool {
    let id = task_id.trim();
    if id.is_empty() {
        return false;
    }
    lock_table().contains(id)
}
