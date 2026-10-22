//! WorkBuddy 的计费域：积分 / 额度查询、签到、运营活动。
//!
//! ── 这一层搬了什么 ──────────────────────────────────────────
//! ```text
//!   usage.rs      积分查询与两种摊平形状（简报 / 逐份明细）—— 逐字保留
//!   ops.rs        签到状态 / 领取 / 一次性奖励 / banner / 大使 —— 逐字保留
//!   checkin.rs    国内版每日签到（协议层 `checkin.claim` 的执行体）
//!   commodity.rs  积分包商品码与套餐分类 —— 逐字保留
//!   tools.rs      JS 语义工具（Number() / parseTime / String(floor(n))）—— 逐字保留
//!   headers.rs    计费接口的头集合（与转发头**不同**）—— 逐字保留
//!   call.rs       一次计费调用（网关 call_billing 的插件侧等价物）
//! ```
//!
//! ── 为什么签到与活动也在这里（2026-10 补迁）─────────────────
//! 端点路径（`/v2/billing/meter/daily-checkin`）、重复领取的上游文案
//! （「今天已签到，请明天再来」）、跨站落点（`www.codebuddy.cn`）都是
//! 「这家上游长什么样」的知识。它们原先留在网关的 `core::billing`，
//! 按项目的风险隔离要求搬到这里；网关只留「目标集合 + 结果记账 + 调度」。
//!
//! ── 输出形状为什么一字不改 ──────────────────────────────────
//! `{kind, unlimited, totalLeft, planLeft, bonusLeft}` 是账号页余额列的既有
//! 契约（前端 `usageSummary` 与后端 `usage_records::extract_remaining` 都认
//! `totalLeft`）。把它包装成「统一钱包形状」会让那一格的显示退化 —— 那是
//! 明令禁止的。所以这里是**加**不是换：五个既有键原样，另挂 `credits[]` 与
//! `subscription` 两项（账号页弹层读）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本目录零 unwrap/expect/panic。

pub mod call;
pub mod checkin;
pub mod commodity;
pub mod headers;
pub mod ops;
pub mod tools;
pub mod usage;

pub use usage::{credit_packs_from, credits_summary_from, query_usage};
