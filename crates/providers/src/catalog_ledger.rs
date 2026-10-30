//! 模型目录的**台账**：把「上一次真拉到的清单」按账号存进网关的账号库。
//!
//! ── 解决什么问题 ────────────────────────────────────────────
//! 各家的目录缓存（`models::cached_catalog` 这类）都活在插件进程的**内存**里，
//! 进程一重启就清零。网关自己那份落盘缓存（`kv` 的 `modelCatalogCache`）只喂
//! 它的 `/v1/models` 与选路 —— 插件拿不到它：协议里没有把目录推回插件的方法，
//! `HostEnv` 也没有目录槽（6 个字段，见 `provider-protocol` 的 `HostEnv`）。
//!
//! 于是重启后的判据只剩两样：内存（空）与各家的**静态兜底清单**。靠目录校验
//! 转发的家（CodeArts / Qoder / Accio）在那一刻会拒发「上游新加的、兜底表里
//! 没有的」模型；CodeArts 连兜底表都没有，于是**所有**模型都被拒
//! （症状：`CodeArts 模型目录还没拉取过：请先在模型页对该账号执行一次「获取模型」`）。
//!
//! 台账是协议里现成的**按账号持久通道**（`store.ledger_get` / `store.ledger_put`）
//! —— CodeArts 的福利台账、豆包工作的模型目录都走它。数据挂在账号记录上：
//! 随账号保存、随账号删除、跨重启与备份恢复都在，不涉及文件落点约定，也不碰
//! 「另一个进程去开网关的库」那类问题。
//!
//! ── 约定 ────────────────────────────────────────────────────
//! * 一份目录一个 `kind`（`<provider>-catalog`），网关会把它存成
//!   `pluginLedger.<kind>`，因此**别跟 `onboarding` 这类既有 kind 撞名**。
//! * 信封里带 `version`：形状变了就升 [`VERSION`]，读不回来的旧信封按「没有
//!   缓存」处理（回落静态兜底或就地重拉），而不是硬塞一份读不懂的状态。
//! * 缓存**不带有效期**：宁可给一份旧清单，也不要退回静态兜底 —— 与网关侧
//!   `catalog_cache` 同一条取舍，时效性交给刷新链路（各家 TTL + 定时任务）。
//!
//! ── 写失败不是致命错误 ──────────────────────────────────────
//! 写不进去只影响「下一次重启能不能提前恢复」，不影响这一次刷新。所以
//! [`save`] 把错误交回调用方，由它决定口径（各家只打一行 verbose 日志）。

use agent2api_provider_sdk::PluginCtx;
use serde_json::{json, Value};

/// 信封版本。改动 `payload` 的形状时 +1。
pub const VERSION: u64 = 1;

/// 从台账读回来的一份目录。
///
/// 返回的就是各家**自己的 payload**（本模块不解释它的形状）。信封里另存了一个
/// `savedAt`（写台账的时刻）只为排障时肉眼可读 —— 「目录是什么时候拉到的」在各家
/// payload 里（CodeArts 是 `fetchedAt`、Qoder / Accio 是按地区的 `*FetchedAt`），
/// 界面「更新日期」要显示的是那个。
pub type Saved = Value;

/// 读一份台账。
///
/// 没有 / 版本不认识 / 没有 `payload` 都返回 `None`（=「没有缓存」）。
/// 读失败（宿主报错）同样返回 `None`：台账只是加速，不该让转发因为一次
/// 存储抖动而失败 —— 退回「就地重拉」那条路即可。
pub async fn load(ctx: &PluginCtx, account: &str, kind: &str) -> Option<Saved> {
    let value = match ctx.ledger_get(account, kind).await {
        Ok(value) => value,
        Err(error) => {
            crate::logging::verbose(
                "[Models]",
                &format!("{kind} 台账读取失败（按没有缓存处理）：{error}"),
            );
            return None;
        }
    };
    let version = value.get("version").and_then(Value::as_u64);
    if version != Some(VERSION) {
        if version.is_some() {
            crate::logging::verbose(
                "[Models]",
                &format!("{kind} 台账版本不认识（{version:?}），按没有缓存处理"),
            );
        }
        return None;
    }
    value.get("payload").filter(|value| value.is_object()).cloned()
}

/// 写一份台账。
///
/// `account` 必须是**真实账号 id**（空串是「队首」的语义，台账按账号存，
/// 不能拿它当键）。
pub async fn save(
    ctx: &PluginCtx,
    account: &str,
    kind: &str,
    payload: &Value,
) -> Result<(), String> {
    if account.trim().is_empty() {
        return Err("台账按账号保存，账号 id 不能为空".to_string());
    }
    let value = json!({
        "version": VERSION,
        "savedAt": crate::logging::now_ms(),
        "payload": payload,
    });
    ctx.ledger_put(account, kind, &value)
        .await
        .map_err(|error| error.to_string())
}
