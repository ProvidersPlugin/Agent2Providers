//! OfficeAce 模型目录：`GET {网关}/v1/models`（OpenAI 格式）+ 持久化缓存。
//!
//! ── 上游形态（依据：officeace2api 的 `server.mjs` 用同一凭据打
//! `{baseUrl}/models`；桌面端 `models.json` 里每条也带 `model_name`）──
//! 标准 OpenAI `{data:[{id, object, created, owned_by}]}`。**只要网关 Basic
//! 凭据**，不需要控制面签名（那套只服务额度/签到）。
//!
//! ── 实测口径：目录 ≠ 可用，而且目录 ≠ 官方展示 ─────────────────
//! officeace2api 实测：上游目录 34 个名，两个号实际只有 11 个能用，其余回
//! `81004`（没权限）/ `81009`（名字不认）。
//!
//! 但比「可用性」更要紧的一层是**官方客户端到底列哪些**：OfficeAce 桌面端
//! 把 `/v2/models` 的完整目录按一张硬编码白名单收窄后才渲染（依据见
//! [`OFFICIAL_WHITELIST`]）。实测这条通道上目录有 26 条、官方界面只列 5~6 条
//! —— 差额全是 `kimi-k3-test-1009` / `glm-5.2-zx` / `openpangu-2.0-pro` /
//! `deepseek-v4.1-flash-bnt` 这类内部灰度名。
//!
//! 本模块**按官方白名单收窄**（[`apply_whitelist`]）。这与「目录照收」是相反
//! 的取舍，理由：用户看到的是「这家官方客户端能用的模型」，把上游的内部实验
//! 名一并广告出去只会让人逐个试错（本仓的 `Fatal` 分类不换家，一次点名到无
//! 权限模型就直接把上游错误透传给客户端）。白名单之外的名字**仍然点名可用**
//! —— 过滤只作用于清单，不改转发闸门。
//!
//! 白名单的代价是**要跟着上游改**：官方客户端每次发版都可能增删这几个名字。
//! 因此留了一条兜底：白名单一条都匹配不上时回落到完整目录并留痕（见
//! [`apply_whitelist`]）—— 上游整体换血时宁可多列几个，也不能把清单清空。
//!
//! ── 缓存 ────────────────────────────────────────────────────
//! **只有进程内一格**（10 分钟 TTL，手动「刷新模型清单」时 `force = true` 绕过）。
//! 插件不落盘 —— 目录缓存归网关：它拿 `models.refresh` 的返回值自己按 provider id
//! 持久化，重启后由网关把清单连同 `models.list` 一起问回来。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use std::sync::{Mutex, OnceLock};

use agent2api_provider_sdk::PluginCtx;
use serde_json::{Value, json};

use crate::http::send_raw;
use agent2api_provider_protocol as proto;

use super::credentials::{from_record, OfficeAceCredential};

/// 自动路径的缓存 TTL（与各家同档）
const TTL_MS: i64 = 10 * 60 * 1000;

/// 进程内缓存
struct CatalogState {
    models: Vec<Value>,
    fetched_at: i64,
}

static STATE: OnceLock<Mutex<Option<CatalogState>>> = OnceLock::new();

fn state() -> &'static Mutex<Option<CatalogState>> {
    STATE.get_or_init(|| Mutex::new(None))
}

fn with_state<R>(f: impl FnOnce(&mut Option<CatalogState>) -> R) -> R {
    let mutex = state();
    let mut guard = mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

/// 当前清单（进程内；为空时由刷新路径填，网关负责持久化它拿到的清单）。
pub fn list() -> Vec<Value> {
    with_state(|slot| slot.as_ref().map(|state| state.models.clone()).unwrap_or_default())
}

/// 这份清单是否来自远程拉取（界面「来源」列的判据）。
pub fn remote_refreshed() -> bool {
    with_state(|slot| {
        slot.as_ref()
            .map(|state| !state.models.is_empty())
            .unwrap_or(false)
    })
}

/// 最近一次成功刷新的时刻（毫秒；没刷过为 0）。
pub fn last_refreshed_at() -> i64 {
    with_state(|slot| slot.as_ref().map(|state| state.fetched_at).unwrap_or(0))
}

/// 每模型倍率的**最后一份非空读数**（`model_name` 小写 → `cost_coefficient`）。
///
/// 为什么要单独留一份，而不是「每次刷新都现取」：倍率走的是控制面那条 V11 签名链
/// （临时 AK/SK，约 2 小时过期），而模型目录走的是**不过期**的网关 Basic —— 两条链寿命不同。
/// 一过期就「取不到 ⇒ 清空」，会让已经有读数的列变成一片 `—`，看起来像「上游不给倍率」，
/// 而真实情况只是我们手上那把临时凭据到期了。宁可旧一点也不假空。
///
/// ⚠️ 这份兜底只在**同一进程内**有效：重启后表是空的，若那一发恰好取不到，这一轮
/// 目录就没有倍率（下一次取到即恢复）。没有把它落到 kv，是因为自动续期正常时这条
/// 路径走不到；真要固化，随目录一起持久化 `credits` 就是它的落点（成功那一轮的
/// 读数本来就在返回给网关的那份清单里）。
static RATES: OnceLock<Mutex<Vec<(String, f64)>>> = OnceLock::new();

fn rates_slot() -> &'static Mutex<Vec<(String, f64)>> {
    RATES.get_or_init(|| Mutex::new(Vec::new()))
}

fn current_rates() -> Vec<(String, f64)> {
    rates_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// 记下这份新读数并返回「本次该用哪一份」：**空表不许把已有读数抹掉**
/// （上游整个挂了与真的没有倍率，回的都是空数组，分不清就不该当"没有"处理）。
fn remember_rates(rows: Vec<(String, f64)>) -> Vec<(String, f64)> {
    if rows.is_empty() {
        return current_rates();
    }
    let mut guard = rates_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = rows.clone();
    rows
}

/// 解析 `{models: [{model_name, cost_coefficient, …}]}`。
///
/// `cost_coefficient` 两种形态都收（JSON 数字与 `"0.30"` 字符串）：上游这条链里
/// 「数值其实是字符串」是**已经踩过一次**的形态（订阅/额度那批字段就是字符串，
/// 见 `subscription.rs`），而倍率解不出来只会静默少一列，不会报错。
/// 没有 `model_name`、数解不出来或**负数**的条目丢掉 —— 负数不是倍率，是脏数据。
pub fn parse_rates(payload: &Value) -> Vec<(String, f64)> {
    let rows = payload.get("models").and_then(Value::as_array);
    let mut out: Vec<(String, f64)> = Vec::new();
    for row in rows.into_iter().flatten() {
        let Some(name) = row.get("model_name").and_then(Value::as_str) else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let value = match row.get("cost_coefficient") {
            Some(Value::Number(number)) => number.as_f64(),
            Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
            _ => None,
        };
        let Some(value) = value.filter(|value| value.is_finite() && *value >= 0.0) else {
            continue;
        };
        out.push((name.to_lowercase(), value));
    }
    out
}

/// 把倍率并进目录条目：写 `credits` 文本（界面那一列读它，与别家同一键同名同格式）。
///
/// ── 只按**精确 id** 匹配，不做前缀借用 ──────────────────────
/// 参考实现的面板 `rateFor()` 允许「表里没有这个 id 就蹭同名前缀」，它自己都警告这
/// 样出来的数是「看起来合理但是错的」（`glm-5.3` 蹭到 `glm-5` 的倍率）。倍率是给人
/// 决定用哪个模型的依据，**错一个数量级比空着坏得多** —— 所以这里宁可留 `—`。
/// 大小写不敏感（目录 id 与倍率表的命名偶有大小写差），分隔符不参与匹配。
pub fn attach_rates(models: Vec<Value>, rates: &[(String, f64)]) -> Vec<Value> {
    if rates.is_empty() {
        return models;
    }
    models
        .into_iter()
        .map(|mut model| {
            let id = model
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_lowercase();
            if let Some((_, value)) = rates.iter().find(|(name, _)| *name == id) {
                if let Some(object) = model.as_object_mut() {
                    object.insert(
                        "credits".to_string(),
                        Value::String(credits_text(*value)),
                    );
                }
            }
            model
        })
        .collect()
}

/// 倍率数字 → 界面文本（`x0.7 credits`）：整数不带小数点，其余最多两位并裁尾零。
/// 两位是必要的 —— 实测读数里有 `0.25` 这种，一位会显示成 `0.3`。
/// 与 `trae::models::credits_text`、`catpaw::catalog::rate_text` 同一取向（本仓第 3 份，
/// 三家的上游给的形态各不相同，先各自保留；要合并得连键名口径一起谈）。
fn credits_text(rate: f64) -> String {
    let rounded = (rate * 100.0).round() / 100.0;
    let plain = if (rounded - rounded.trunc()).abs() < 1e-9 {
        format!("{}", rounded.trunc() as i64)
    } else {
        format!("{rounded:.2}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    };
    format!("x{plain} credits")
}

/// 官方客户端展示的模型白名单（`id` 小写；比对时也按小写）。
///
/// ── 依据 ────────────────────────────────────────────────────
/// OfficeAce 桌面端 `packages/api/dist/chunk-UEENO6BL.js`（39.8.7 实测）里
/// 的 model-selection candidates 那条链：拉完 `/v2/models` 后逐条比对
/// `Ct = new Set(["GLM-5","GLM-5.1","GLM-5.2","GLM-5.3",
/// "DeepSeek-V4-Flash-0731","DeepSeek-V4.1-Flash"])`，不在集合里的直接丢
/// （`Et()` 的 `!c && !l ? [] : [...]`）。同一文件里还有两张小表：
///   · `$r` = `deepseek-v3-64k-batch` / `deepseek-r1-batch` / `deepseek-r1-batchl`
///     —— 三个 batch 名，无条件剔除（本白名单天然不含它们，无需另写）；
///   · `-zx` 后缀在比对前剥掉（`_t()`），即 `glm-5.2-zx` 会按 `glm-5.2` 命中。
///     本模块照这个口径实现（见 [`canonical_id`]）。
///
/// ── 为什么是「名字」而不是「能力」──────────────────────────
/// 这张表是**官方产品的展示决策**，不是上游能力声明：`kimi-k3.1-pro` 很可能
/// 真的能打，只是官方客户端没打算让用户选它。所以这里只过滤清单，不做门禁
/// —— 白名单外的名字点名调用仍然会被转发（见模块头）。
const OFFICIAL_WHITELIST: &[&str] = &[
    "glm-5",
    "glm-5.1",
    "glm-5.2",
    "glm-5.3",
    "deepseek-v4-flash-0731",
    "deepseek-v4.1-flash",
];

/// 白名单的规范形态：剥掉 `-zx` 后缀（官方客户端 `_t()` 的同一处理）。
///
/// ── 为什么剥掉后缀之后要**真的改名**（而不是只用于比对）──────
/// 官方客户端 `_t()` 返回的是剥掉后缀的名字，`Et()` 直接把它当 `model` 用
/// （`model: d`），随后 `Vr()` 还以这个名字为 key 做去重 —— 也就是
/// `glm-5.2-zx` 在官方眼里**就是** `glm-5.2`，两者合并成一条。
///
/// 这个处理有独立的实测支撑：本仓 2026-10-23 的探测记录（`kv` 的
/// `officeaceModelProbe`）里，`glm-5.2-zx` 与 `glm-5.3-zx` 都被上游判成
/// 「模型级拒绝」（81004/81009），而同名的 `glm-5.2` / `glm-5.3` 正常可用。
/// 也就是说 `-zx` 这个名字很可能上游不认 —— 只比对不改名会把一条打不通的
/// 名字广告出去。（⚠️ 那份记录来自旧网关实现，本次改动没有再探一轮验证。）
///
/// 别的后缀（`-bnt` / `-harmony` / `-0930` / `-1017`）**不剥**：官方实现里
/// 只有 `-zx` 一个特判，其余是独立变体，归并会让两个不同的上游模型撞成一条。
fn canonical_id(id: &str) -> &str {
    let trimmed = id.trim();
    match trimmed.len().checked_sub(3) {
        Some(cut) if trimmed.to_ascii_lowercase().ends_with("-zx") => &trimmed[..cut],
        _ => trimmed,
    }
}

/// 按官方白名单收窄目录，并把 `-zx` 归并到主名。
///
/// ── 一条兜底：一条都匹配不上时**原样返回** ──────────────────
/// 白名单是外部事实（官方客户端的硬编码），上游换血时它会整体失效。那种情况
/// 下如果照白名单过滤，清单会变成空的 —— 而「这家一个模型都没有」在界面上
/// 与「插件坏了」无法区分。所以命中 0 条时退回完整目录，并打一条日志留痕
/// （排查时能看出「白名单该更新了」，而不是以为上游真没模型）。
///
/// ── 去重顺序：主名优先 ──────────────────────────────────────
/// `glm-5.2` 与 `glm-5.2-zx` 归并后是同一条。目录里两者都在时保留**不带
/// 后缀**的那条（它带的是上游对外的正式字段）；只有 `-zx` 时就用归并后的
/// 主名顶上。与官方 `Vr()` 里 `isZx && !f` 的那段判断同一取舍。
pub fn apply_whitelist(models: Vec<Value>) -> Vec<Value> {
    let total = models.len();
    // 归并后的清单：(小写主名, 这条是不是 `-zx` 变体, 条目)。
    // 第三个字段单独记「来源是变体」—— 变体在下面会被改名成主名，改完之后
    // 光看 id 已经分不出它原本是 `glm-5.2` 还是 `glm-5.2-zx` 了。
    let mut kept: Vec<(String, bool, Value)> = Vec::new();
    for entry in models.iter() {
        let mut model = entry.clone();
        let id = model.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let canonical = canonical_id(&id).to_string();
        let key = canonical.to_ascii_lowercase();
        if !OFFICIAL_WHITELIST.iter().any(|name| *name == key) {
            continue;
        }
        let is_variant = canonical != id;
        if is_variant {
            // `-zx` 变体：改名到主名（见 `canonical_id` 的说明）。上游不认这个
            // 后缀名（实测回 81004/81009），广告出去只会让用户点到打不通的名字。
            if let Some(object) = model.as_object_mut() {
                object.insert("id".to_string(), Value::String(canonical.clone()));
                object.insert("name".to_string(), Value::String(canonical.clone()));
            }
        }
        match kept.iter().position(|(existing, _, _)| *existing == key) {
            // 已有同名条目：只有「已存的是变体、当前是主名」才顶替，其余丢掉。
            Some(index) => {
                if kept[index].1 && !is_variant {
                    kept[index] = (key, false, model);
                }
            }
            None => kept.push((key, is_variant, model)),
        }
    }
    if kept.is_empty() {
        // 白名单一条都没命中 ⇒ 上游大概率改了命名。退回完整目录并留痕：
        // 空清单在界面上与「插件坏了」无法区分，而多列几个内部名只是吵。
        crate::logging::verbose(
            "[OfficeAce]",
            &format!(
                "官方展示白名单一个都没命中（目录共 {total} 条），本次按完整目录返回 —— 上游可能改了命名，白名单需要跟着更新"
            ),
        );
        return models;
    }
    kept.into_iter().map(|(_, _, model)| model).collect()
}

/// 把一个上游条目映成目录条目（保留上游给的字段，缺的补 `officeace`）。
fn map_entry(entry: &Value) -> Option<Value> {
    let id = entry
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if id.is_empty() {
        return None;
    }
    let mut out = json!({
        "name": id,
        "display_name": entry
            .get("display_name")
            .and_then(Value::as_str)
            .unwrap_or(id),
        "description": "",
        "context_length": entry
            .get("context_window")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        // 目录不给输出上限（officeace2api 的 `/v1/models` 也没这个字段）；
        // 留 0 = 「未声明」，界面显示 `—`，不编造。
        "max_output_tokens": 0,
        "credit_display": "",
        "id": id,
    });
    if let Some(object) = out.as_object_mut() {
        // `owned_by` 原样透传（我们的目录形状里叫 `owned_by` 的不多，留着不碍事）
        if let Some(owner) = entry.get("owned_by").and_then(Value::as_str) {
            object.insert("owned_by".to_string(), Value::String(owner.to_string()));
        }
        // 能力位：上游这条链**不报**（officeace2api 的目录也不带 vision/tool 位），
        // 所以不写 `supportsImages` —— 缺席 = 「不知道」，闸门据此不拦（见
        // `catalog/routing.rs` 的 `entry_refuses_images` 与它的模块头）。
        if let Some(developer) = entry.get("developer").and_then(Value::as_str) {
            object.insert("developer".to_string(), Value::String(developer.to_string()));
        }
    }
    Some(out)
}

/// 解析 `{data:[…]}` 或裸数组。
pub fn parse_models(payload: &Value) -> Vec<Value> {
    let rows = payload
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| payload.as_array())
        .cloned()
        .unwrap_or_default();
    rows.iter().filter_map(map_entry).collect()
}

/// 拉一次模型目录并落地（内存 + 持久化缓存）。
pub async fn refresh(ctx: &PluginCtx, credential: &OfficeAceCredential, force: bool) -> proto::RefreshOutcome {
    let now = crate::logging::now_ms();
    if !force {
        let fresh = with_state(|slot| {
            slot.as_ref()
                .map(|state| !state.models.is_empty() && now - state.fetched_at < TTL_MS)
                .unwrap_or(false)
        });
        if fresh {
            return proto::RefreshOutcome::unchanged();
        }
    }
    let url = format!("{}/models", super::chat::normalize_base(&credential.base_url));
    if url == "/models" {
        return proto::RefreshOutcome::failed("OfficeAce 账号缺少模型网关基址");
    }
    let headers = vec![
        (
            "Authorization".to_string(),
            super::chat::basic_authorization(&credential.model_app_key, &credential.model_app_secret),
        ),
        ("Accept".to_string(), "application/json".to_string()),
    ];
    let response = match send_raw(
        "GET",
        &url,
        None,
        &headers,
        None,
        Some(20_000),
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            let text = if error.is_timeout() {
                "OfficeAce 模型目录查询超时".to_string()
            } else {
                format!("OfficeAce 模型目录查询失败：{error}")
            };
            return proto::RefreshOutcome::failed(text);
        }
    };
    if !response.ok {
        return proto::RefreshOutcome::failed(format!(
            "OfficeAce 模型目录返回 HTTP {}（网关不接受这条凭据？）",
            response.status
        ));
    }
    let models = parse_models(&response.payload.unwrap_or(Value::Null));
    if models.is_empty() {
        return proto::RefreshOutcome::failed("OfficeAce 模型目录为空或格式不认识（保留现有清单）");
    }
    // 按官方客户端那张白名单收窄（见 `apply_whitelist`）：上游目录里有大量
    // 内部灰度名（`*-test-*` / `*-zx` / `*-bnt`），官方界面从不展示它们。
    // 收窄在**倍率合并之前**做：倍率表是另一条链给的，两边的集合本来就对不齐，
    // 先收窄能少算一轮无关条目。
    let upstream_count = models.len();
    let models = apply_whitelist(models);
    crate::logging::log(
        "[OfficeAce]",
        &format!(
            "模型目录：上游 {} 条，官方展示白名单保留 {} 条",
            upstream_count,
            models.len()
        ),
    );
    // 倍率：另一条链（控制面签名），失败就沿用最后一份读数，别抹空（见 `RATES`）。
    let rates = match super::subscription::fetch_model_token_rates(credential).await {
        Ok(payload) => remember_rates(parse_rates(&payload)),
        Err(error) => {
            crate::logging::verbose(
                "[OfficeAce]",
                &format!("模型倍率没取到，沿用上一份读数：{}", error.message),
            );
            current_rates()
        }
    };
    let models = attach_rates(models, &rates);
    // 一条事实留痕：上游给了几条读数、几条对上了目录。少了这条，「只有 4 个模型显示倍率」
    // 就分不清是**上游只发布这几个**还是**两边命名对不上**（后者才需要改匹配规则）。
    let with_rate = models.iter().filter(|model| model.get("credits").is_some()).count();
    crate::logging::log(
        "[OfficeAce]",
        &format!(
            "模型倍率：上游给了 {} 条读数，对上目录 {} 条（目录共 {} 条）",
            rates.len(),
            with_rate,
            models.len()
        ),
    );
    let count = models.len();
    with_state(|slot| {
        *slot = Some(CatalogState {
            models: models.clone(),
            fetched_at: now,
        });
    });
    // 探测完成后通知网关同步清单，不再请求上游目录。
    super::probe::maybe_spawn(ctx, credential, &models, force);
    proto::RefreshOutcome::refreshed(count)
}

/// 从账号记录直接刷（适配器用）。
pub async fn refresh_from_record(ctx: &PluginCtx, record: Option<&Value>, force: bool) -> proto::RefreshOutcome {
    let credential = match from_record(record) {
        Ok(credential) => credential,
        Err(error) => return proto::RefreshOutcome::failed(error.message),
    };
    refresh(ctx, &credential, force).await
}

