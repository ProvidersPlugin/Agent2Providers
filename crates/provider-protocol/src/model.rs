//! 模型目录：记录形状、刷新结果、缓存元数据。
//!
//! ── 记录为什么是 `Value` 而不是强类型 ───────────────────────
//! 网关的模型清单要透传上游原始形态（`/v3/config` 的 `models[]`、
//! 小浣熊的 `/model_catalog` 条目……），再由 `models::list_item` 做字段映射
//! 与 `model_meta` 补能力位。把上游字段逐个建模会让「上游加了一个字段」
//! 变成「插件要改代码 + 升协议版本」。
//!
//! 代价是插件可能给出一条网关不认识形状的记录。这由网关侧兜底：解析不出
//! `id` 的记录被丢弃并记一条警告，不会让整份目录失效 —— 与今天
//! `catalog::manifest_for` 的容错口径一致。

use serde::{Deserialize, Serialize};

/// 一条模型记录（上游原始形态的 JSON）。
///
/// 约定：**至少**要有 `id` 字段（字符串）。其余字段由网关按需读取，
/// 缺失时走默认值。
pub type ModelRecord = serde_json::Value;

/// `models.refresh` 方法的返回。
///
/// ── 三个字段各表达什么（不要合并）─────────────────────────────
///   - `refreshed`：这次**真的**落地了一份新清单；
///   - `count`：落地后的条目数（仅 `refreshed = true` 时有意义）；
///   - `message`：`Some` = 尝试了但失败（保留旧清单）；`None` = 成功或
///     按缓存跳过。
///
/// 「按缓存跳过」与「失败」必须分开：前者是正常行为（自动刷新每几分钟跑
/// 一次，不该每次都真打上游），后者要显示给用户。合并成一个 `bool` 会让
/// 界面在缓存命中时报「刷新失败」。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RefreshOutcome {
    /// 是否真的落地了一份新清单。
    pub refreshed: bool,
    /// 落地后的目录条目数。
    #[serde(default)]
    pub count: usize,
    /// 失败原因（`None` = 没失败）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl RefreshOutcome {
    pub fn refreshed(count: usize) -> Self {
        Self { refreshed: true, count, message: None }
    }

    pub fn unchanged() -> Self {
        Self::default()
    }

    pub fn failed(reason: impl Into<String>) -> Self {
        Self { refreshed: false, count: 0, message: Some(reason.into()) }
    }
}

/// 目录的刷新元数据（`models.meta` 方法的返回）。
///
/// ── 为什么需要它 ────────────────────────────────────────────
/// 管理页的「来源 / 更新日期」两列要如实显示「这份清单是内置的，还是某次
/// 从上游拉回来的、什么时候拉的」。这两个值是**各家的缓存状态**，网关
/// 自己不知道，只能问插件。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CatalogMeta {
    /// 当前清单是不是远程拉回来的（`false` = 内置静态兜底）。
    pub remote: bool,
    /// 上次成功刷新的时刻（毫秒时间戳）；`0` = 从没刷过。
    pub refreshed_at: i64,
}

/// 模型清单的返回（`models.list`）。
///
/// ── 为什么清单与元数据一起回 ────────────────────────────────
/// 网关的 `/v1/models` 与「模型管理」页要同时用两者；分两次 RPC 会让
/// 「清单已换、元数据还是上一次」的中间态被界面读到。一次返回是原子的。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelList {
    /// 上游原始形态的记录。
    pub models: Vec<ModelRecord>,
    /// 这份清单的缓存元数据。
    #[serde(default)]
    pub meta: CatalogMeta,
}
