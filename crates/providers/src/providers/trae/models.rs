//! Trae SOLO 的模型目录（`POST /api/ide/v1/get_detail_param`）。
//!
//! ── 为什么这张表**只能**远程拉 ──────────────────────────────
//! 上游目录不是一张静态清单，而是"按客户端版本给的一张表"：
//! `X-Ide-Version-Code` 决定拿到哪一张（参考实现实测 `20260716` → 35 条且
//! 没有 `glm-5.3`、`20260820` → 36 条含）。把它抄成常量表就等于把一个时间点
//! 的读数当成契约，上游加一个模型我们要改代码发一版才行。所以本模块
//! **没有静态兜底**：没刷到就是空清单。
//!
//! 空清单的效果要如实理解：广告视图里没有本家模型 ⇒ 客户端点不动
//! （入口校验以广告视图为准），而不是"点了报错"。这与另外几家「静态兜底 +
//! 远程覆盖」的结构不同，别照抄它们的 `FALLBACK`。
//!
//! ── 目录里的三类非可选条目（v0.12.46 对 38 条实测目录校准）──────
//! 上游把三种东西和正式模型混在一张表里，全部要滤掉：
//!   - `is_invisible_to_user = true`：内部 subagent / 实验通道
//!     （`browser_use_subagent`、`file_search_agent`、`sagitta/aquila`…）；
//!   - `display_config.display_name` 为空：租户自定义模型的**占位模板**
//!     （`custom_model_*`、`summary` 等），它们能出现在表里但不是可选模型；
//!   - `config_switch = false`：已下线开关。
//!
//! 三个可布尔字段一律"缺省 = 可见 / 启用"（上游删字段时不能把整张表滤空 ——
//! 那是最坏的失效方式：看起来像"上游没模型了"，其实是本机判空判错了）。
//! 外加 [`errors::config_is_solo_agent_only`] 那张死配置名单：那些 config 在
//! `solo_work_lite` 通道必定流内 `4001`（属于 IDE 加密 agent 通道 /
//! `llm_raw_chat` 的 `solo_agent`），注册出来只会让用户点到必然失败的模型。
//!
//! ── 广告名带 `-solo` 后缀 ───────────────────────────────────
//! 本家的 config 名与别家会撞（`glm-5.2`、`minimax-m3` 在 ZCode 那张静态表里
//! 就有；聚合清单按"先到先得"去重，撞名那家会从 `/v1/models` 整个消失）。
//! 所以广告出去的 id 是 `<config_name>-solo`，出站前由
//! [`payload::sanitize_model_name`] 剥回裸名 —— 与参考实现同款约定，也只剥
//! **一层**（真以 `-solo` 结尾的 config 仍然往返得回来）。
//!
//! ── 输出上限与能力位：写"上游真给的数" ──────────────────────
//! 目录里 `model_detail_list[].encrypted_model_params` 确实是**密文**（没密钥解不开，
//! 参考实现因此把 `MaxTokens` 恒记 0），但**同层就有明文的 `max_tokens`**
//! （本机实测：17 个可见 config 里 16 个 32000、`agnes-2.5-flash` 16000）——
//! 参考实现没读它，不等于上游没给。同理还有 `display_config.model_capability`
//! （`reasoning_model` / `chat_model`）与 `extra_config.native_function_call`。
//! 这三条本家都读出来广告；仍然守同一条纪律：**上游没给的键就不写**，
//! 而不是补一个 0 —— 写 0 进 `/v1/models` 会被客户端当真上限，
//! 进而把 `max_output_tokens: 0` 发给上游。
//!
//! `display_config.multimodal`（识图能力）**也是端到端实测确认过才接的**：
//! 同一张 64×64 双色 PNG（上半红、下半蓝）以 `data:` URL 送进本家这条
//! `solo_work_lite` 通道，判据是"答对颜色的只有标 true 的那几个"。
//!   * `kimi-k2.6`（true）→ `上=红，下=蓝`（prompt 45）；
//!   * `minimax-m3`（true）→ `红色，蓝色`（prompt 203，图片真进了 token 账）；
//!   * `DeepSeek-V4-Flash-Official`（false）→ HTTP 200 但答 `上=未知，下=未知`
//!     ——**这才是最坏的一种**：客户端会以为模型看了图；
//!   * `glm-5.2`（false）→ 流内 `code=3004`，**同一条 body 带图必现、改成纯文本
//!     立刻 200** ⇒ 那句 "exceeded the rate limit" 是假文案，别照它把 3004
//!     映射成限流档（那会让宿主拿同一条必然失败的 body 去轮询其他账号）。
//!
//! ⇒ 上游这个标注在本通道上**有预测力**（2 真全对 / 3 假全废），所以接。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic；持锁期间不做网络。

use std::collections::HashSet;
use std::sync::{OnceLock, RwLock};

use serde_json::{Value, json};

use agent2api_provider_protocol::RefreshOutcome as ModelRefreshOutcome;
use crate::logging;
use agent2api_provider_protocol::WireProxy as ResolvedProxy;

use super::credentials::Credential;
use super::errors::config_is_solo_agent_only;
use super::headers::{IDE_VERSION_CODE, solo_headers, HeaderIdentity};
use super::http::{Reply, post_json};
use super::{AGENT_BASE_URL, MODELS_PATH};

/// 远程目录缓存有效期（与另外几家同为 1 小时；上游改表不需要秒级可见）。
const CACHE_TTL_MS: i64 = 60 * 60 * 1000;

/// 广告 id 的后缀（见模块头"广告名带 `-solo` 后缀"）。
pub const ADVERTISE_SUFFIX: &str = "-solo";

/// Max 模式变体的广告后缀：`<config>-solo-max`。
///
/// ── 为什么要造一个变体（而不是让用户切开关）────────────────
/// Max 模式在上游是**请求体里的两个字段**（`model_auto_selection.strategy =
/// "max"` 与 `context_window_size`），模型名不变；而本网关对客户端暴露的只有
/// `/v1/models` 这张清单，没有"给某个模型开个开关"的通道。所以把一个模型能开
/// Max 这件事**编码进 id**：清单里多一行 `<config>-solo-max`，客户端选中它就
/// 等于选了 Max —— 与参考实现（magpie 的 `-max` 后缀）同一套做法。
///
/// 窗口值取 [`MAX_CONTEXT_WINDOW`]：目录接口（`get_detail_param`）里的
/// `context_window_tokens.max` **实测恒等于 `dev`**（49 条全如此），不能拿来当
/// Max 窗口；而客户端模型列表接口里的 `context_window_size.max` 我们这条链路
/// 拿不到。所以这里用一个常量，见该常量的说明。
pub const MAX_ADVERTISE_SUFFIX: &str = "-max";

/// Max 变体广告出去的上下文窗口（1M）。
///
/// 上游对 `context_window_size` 的语义是"这次请求按多大的窗口计费/裁剪"，值由
/// 客户端在 Max 模式下自己带。Trae 的 Max 档对外宣传就是 1M，这里照抄。
/// 取不到真实值时的代价：带得比上游实际支持的大，上游会按自己的上限裁剪（不会
/// 报错）；带得小则白瞎了 Max 档。所以宁大勿小。
pub const MAX_CONTEXT_WINDOW: i64 = 1_000_000;

#[derive(Clone, Default)]
struct Cache {
    models: Vec<Value>,
    fetched_at: i64,
}

static SLOTS: OnceLock<RwLock<Cache>> = OnceLock::new();

/// 内存里的那张表。
///
/// ── 迁移时丢了什么：**持久化缓存** ──────────────────────────
/// 网关那版把每次成功刷到的目录写进 `catalog_cache`（按 SCOPE 落盘），进程
/// 重启后先读回它 —— 好处是「重启那一次刷新失败也不会让本家模型全消失」。
/// 插件侧没有那个缓存目录（`paths::data_dir()` 存在但没有稳定的落点约定，
/// 见 `paths` 模块头），所以这里只留内存：**重启后首轮刷新必须真打上游**。
/// 后果可控：目录按账号刷（`refresh_uses_account`），网关会在拿到账号后
/// 自动刷一轮，且失败时界面显示的是「未刷新」而不是「没有模型」。
fn slot() -> &'static RwLock<Cache> {
    SLOTS.get_or_init(|| RwLock::new(Cache::default()))
}

fn snapshot() -> Cache {
    slot().read().unwrap_or_else(|error| error.into_inner()).clone()
}

/// 本家的清单（聚合目录认的形态）。
pub fn list() -> Vec<Value> {
    snapshot().models
}

/// 是不是刷过（界面的「来源」列：刷过=远程，没刷=内置/空）。
pub fn remote_refreshed() -> bool {
    !snapshot().models.is_empty()
}

pub fn last_refreshed_at() -> i64 {
    snapshot().fetched_at
}

/// 目录请求体（七个键，顺序无所谓 —— 上游按 JSON 对象读）。
///
/// `need_prompt:false` + `poly_prompt:true` 是官方客户端的取值：前者让它别把
/// 每个模型的 prompt 一起回传（体积），后者要的是多段 prompt 结构。参考实现
/// 逐字这么发，向量里也原样记着，别"顺手优化"成别的组合。
pub fn catalog_body(variant: &str) -> Value {
    json!({
        "function": super::payload::function_for(variant),
        "config_names": Value::Null,
        "need_prompt": false,
        "current_config_info": Value::Null,
        "poly_prompt": true,
        "mode_type": Value::Null,
        "agent_type": Value::Null,
    })
}

/// 目录 URL。
pub fn catalog_url() -> String {
    format!("{AGENT_BASE_URL}{MODELS_PATH}")
}

/// 上游响应 → 清单（**纯函数**，由向量 `catalog` 段钉住）。
///
/// 条目顺序保留上游给的顺序：`/v1/models` 的阅读顺序与官方客户端一致，
/// 而且"顺序变了"本身就是一条可观察的漂移信号。
///
/// ── 按 `config_name` 去重（修复 #177）────────────────────────
/// 上游 `config_info_list` 里同一条 `config_name` 可能给两遍（实测
/// `deepseek-v4.1-flash-solo`）：逐条 push 会让管理页画出两行同 id 的模型
/// （第二行是条"没信息"的残条），连带 `manage_view` 为每条模型合成的那条
/// 默认映射也重复一条。留第一条（忽略大小写 —— 与下游按名匹配的
/// `eq_ignore_ascii_case` 同一口径），重复条目直接丢。
pub fn parse_catalog(payload: &Value) -> Vec<Value> {
    let Some(entries) = payload.get("config_info_list").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(entries.len());
    let mut seen: HashSet<String> = HashSet::with_capacity(entries.len());
    for entry in entries {
        let config_name = text(entry, "config_name");
        if config_name.is_empty() {
            continue; // 空 id 的条目不能路由，也不能广告
        }
        if !seen.insert(config_name.to_lowercase()) {
            continue; // 同一条 config_name 的重复条目（见函数头「按 config_name 去重」）
        }
        // `false` 才过滤；缺省（None）按启用处理（见模块头）
        if entry.get("config_switch").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        if entry.get("is_invisible_to_user").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let display_name = entry
            .get("display_config")
            .map(|config| text(config, "display_name"))
            .unwrap_or_default();
        if display_name.is_empty() {
            continue; // 租户自定义占位模板
        }
        if config_is_solo_agent_only(&config_name) {
            continue; // 这条通道必死
        }
        let context = entry
            .get("context_window_tokens")
            .and_then(|window| window.get("dev"))
            .and_then(value_as_i64)
            .unwrap_or(0);
        let mut item = catalog_entry(&config_name, &display_name, entry);
        // 只有目录真给了这些数才写对应的键（缺值写 0 = 谎报上限，见模块头）。
        if let Some(object) = item.as_object_mut() {
            if context > 0 {
                object.insert("maxInputTokens".to_string(), Value::from(context));
            }
            if let Some(output) = max_output_tokens(entry) {
                object.insert("maxOutputTokens".to_string(), Value::from(output));
            }
            if let Some(reasoning) = supports_reasoning(entry) {
                object.insert("supportsReasoning".to_string(), Value::from(reasoning));
            }
            if let Some(vision) = supports_images(entry) {
                object.insert("supportsImages".to_string(), Value::from(vision));
            }
            // 倍率：取不到就**不写这个键**（界面那一列显示 `—`），编一个 `x1` 充数
            // 会让「上游没给」被读成「这个模型不收倍率」。
            if let Some(rate) = consumption_rate(entry) {
                object.insert("credits".to_string(), Value::String(credits_text(rate)));
            }
        }
        // Max 变体：同一份能力位，只把窗口换成 1M、id 加 `-max`。
        // 判定见 [`max_variant_of`]（决定哪些模型该有 Max 档）。
        if let Some(mut variant) = max_variant_of(&item, context) {
            if let Some(object) = variant.as_object_mut() {
                object.insert("maxInputTokens".to_string(), Value::from(MAX_CONTEXT_WINDOW));
            }
            out.push(variant);
        }
        out.push(item);
    }
    out
}

/// 给一个已构造好的模型条目生成 Max 变体；不该有 Max 档时返回 `None`。
///
/// ── 判据：窗口 ≥ [`MAX_VARIANT_FLOOR`] 才算「值得开 Max」──────
/// 上游目录里没有"这个模型支持 Max"的字段（`display_config.max_mode` 实测只有
/// 两条内部 subagent 为 true，与 Max 档无关）。所以按窗口分档：本家目录里
/// 主流是 200k（25 条）、豆包 256k（6 条）、`custom_model_gemini` 300k，
/// 再往上就是 1M 那几个槽位。取 256k 当门槛，让豆包与 1M 档都能拿到 Max，
/// 200k 那批不生成 —— 给 200k 的模型也挂一个 1M 变体，等于替上游承诺它做不到
/// 的事，用户选中后要么被上游截断、要么直接报错。
///
/// 真机上若发现某个 200k 模型也能开 Max，改这个常量即可，不必动结构。
const MAX_VARIANT_FLOOR: i64 = 256_000;

fn max_variant_of(item: &Value, context: i64) -> Option<Value> {
    if context < MAX_VARIANT_FLOOR {
        return None;
    }
    let object = item.as_object()?;
    let id = object.get("id").and_then(Value::as_str)?;
    let name = object.get("name").and_then(Value::as_str).unwrap_or(id);
    let mut variant = item.clone();
    let target = variant.as_object_mut()?;
    // id 在 `-solo` 之后再加 `-max`：`<config>-solo-max`。
    // `payload::sanitize_model_name` 会按相反顺序把两段都剥掉。
    target.insert("id".to_string(), Value::String(format!("{id}{MAX_ADVERTISE_SUFFIX}")));
    target.insert("name".to_string(), Value::String(format!("{name} (Max)")));
    Some(variant)
}

/// 每个模型的积分倍率：藏在条目的 `display_contact_config` 里 —— 那是**一段 JSON 字符串**，
/// 要二次解析（与同一条目上的 `extra_config` 同一形态）。
///
/// 形状与三道坑抄自参考实现 Go 那份 `readConsumptionRate`（`plugins/trae/models.go:200-232`，
/// 它引 Trae 客户端 `trae.ts:851-864`）：
///   ① 键名两种形态都在用（`consumption_rate` / `ConsumptionRate`、`data` / `Data`），
///     只认一种就会静默变成「没给」；
///   ② **`enable:false` 的意思是「这一条没有倍率」**，不是 0 —— 把 `enable` 当装饰读，
///     就会给没有倍率概念的模型编出一个「0 倍（免费）」来；
///   ③ **`rate:0` 是合法值**（免费/限免档），所以不许写成「非零才算有」那种过滤。
///
/// 数值两种形态都收（JSON 数字与 `"0.08"` 字符串）：这类字段上游改过形态是有前科的
/// （别家目录里 `context_window` 就混着字符串），而倍率一旦解析失败就**静默**变成空列，
/// 现场看不出区别。任何一步解不出来都只当「没给」，全程不报错。
fn consumption_rate(entry: &Value) -> Option<f64> {
    let raw = entry.get("display_contact_config").and_then(Value::as_str)?;
    let config: Value = serde_json::from_str(raw.trim()).ok()?;
    let rate = object_of(&config, &["consumption_rate", "ConsumptionRate"])?;
    if rate.get("enable").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let data = object_of(rate, &["data", "Data"])?;
    let value = number_of(data, &["rate", "Rate"])?;
    if value.is_finite() && value >= 0.0 {
        Some(value)
    } else {
        None
    }
}

/// 按候选键名取第一个**对象**字段（两种大小写形态共用一份判据）。
fn object_of<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .filter_map(|key| value.get(*key))
        .find(|found| found.is_object())
}

/// 按候选键名取第一个数（数字与数字字符串都算，负数与非有限值算没有）。
fn number_of(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| match value.get(*key) {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
        _ => None,
    })
}

/// 倍率数字 → 界面那一列的文本（`x0.5 credits`）—— 与小浣熊/CodeArts 同一措辞，
/// 数字格式化也同一取向：整数不带小数点，其余最多两位并裁尾零
/// （目录里 `0.25` / `0.75` 真实存在，一位小数会把 `0.25` 说成 `0.3`）。
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

/// 一条目录条目（聚合形态）。
///
/// `supportsToolCall` 取**上游说的**而不是我们硬编码的：`extra_config` 里带
/// `native_function_call` 键时以它为准（实测 17 个可见 config 里 16 个 true，
/// `agnes-2.5-flash` 整个 `extra_config` 没这个键），缺键仍按 true —— 本家这条
/// 通道确实收 tools（出站 body 里那四处 SOLO 专属变形就是为它做的，见
/// `payload.rs`），而"上游没写"不等于"不支持"（与三个布尔字段同一取向）。
fn catalog_entry(config_name: &str, display_name: &str, entry: &Value) -> Value {
    json!({
        "id": format!("{config_name}{ADVERTISE_SUFFIX}"),
        "name": display_name,
        "supportsToolCall": native_function_call(entry).unwrap_or(true),
    })
}

/// `extra_config` 是**一个 JSON 字符串**（上游就这么塞的），取里面的
/// `native_function_call`。解析失败/没这个键都给 `None`（= 上游没说）。
fn native_function_call(entry: &Value) -> Option<bool> {
    let raw = entry.get("extra_config").and_then(Value::as_str)?;
    let parsed: Value = serde_json::from_str(raw).ok()?;
    parsed.get("native_function_call").and_then(Value::as_bool)
}

/// 输出上限：`model_detail_list[].max_tokens`（**明文**，与同层的
/// `encrypted_model_params` 不同 —— 那份是密文，这一份直接可读）。
///
/// 一个 config 可能有多条 detail（实测全部是租户 `custom_model_*` 的
/// `__max` / `__dev` 两个档位，且两条的 `max_tokens` 相同）。取**最小值**：
/// 广告出去的上限必须两条都能用，取大的等于把客户端送到另一个档位去撞墙。
pub fn max_output_tokens(entry: &Value) -> Option<i64> {
    let details = entry.get("model_detail_list").and_then(Value::as_array)?;
    details
        .iter()
        .filter_map(|detail| detail.get("max_tokens").and_then(value_as_i64))
        .filter(|value| *value > 0)
        .min()
}

/// 思考能力：`display_config.model_capability` 是 `"reasoning_model"` /
/// `"chat_model"` / 空串三态（实测 45 条里 30 / 3 / 12）。
///
/// 空串给 `None`（= 上游没说，不写这个键），不要写成 false —— 我们与上游口径
/// 一致的前提是"没说过的事不替它说"。
/// ⚠️ 这与 `reasoning_effort_config.support_thinking` 是**两件事**：前者是
/// "这个模型会不会输出思考链"，后者是"官方客户端给不给这个账号开思考强度开关"
/// （实测本账号 45 条全 false，属账号权益而非模型属性）。
pub fn supports_reasoning(entry: &Value) -> Option<bool> {
    let capability = entry.get("display_config").map(|config| text(config, "model_capability")).unwrap_or_default();
    match capability.as_str() {
        "reasoning_model" => Some(true),
        "chat_model" => Some(false),
        _ => None,
    }
}

/// 识图能力：`display_config.multimodal` 是**三态**（true / false / 缺键）。
///
/// 缺键给 `None`（= 上游没说，就不写这个键）而不是 false —— 本机实测 45 条里
/// 就有 1 条整个不带这个字段，替它说"不能看图"与本家一直防的"缺省当假"是
/// 同一类错。取值本身经端到端实测，见模块头那张对照。
pub fn supports_images(entry: &Value) -> Option<bool> {
    entry.get("display_config").and_then(|config| config.get("multimodal")).and_then(Value::as_bool)
}

fn text(object: &Value, key: &str) -> String {
    object.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

fn value_as_i64(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_f64().map(|number| number as i64))
}

/// 拉一次远程目录并落地（内存 + 持久化缓存）。
///
/// TTL 早退由调用方是否 `force` 决定（自动路径 `false`、用户点"获取模型" `true`）
/// —— 缓存该不该复用只由**谁发起**决定，与另外几家同一契约。
pub async fn refresh(
    credential: &Credential,
    proxy: Option<&ResolvedProxy>,
    force: bool,
) -> ModelRefreshOutcome {
    let current = snapshot();
    if !force && current.fetched_at > 0 && logging::now_ms() - current.fetched_at < CACHE_TTL_MS {
        return ModelRefreshOutcome::unchanged();
    }
    if !credential.valid() {
        return ModelRefreshOutcome::unchanged();
    }
    let identity = HeaderIdentity {
        access_token: credential.access_token.trim(),
        uid: credential.uid.trim(),
        machine_id: credential.machine_id.trim(),
        device_id: credential.device_id.trim(),
    };
    // 头名是**本家自己造**的字符串（`solo_headers` 返回 BTreeMap），所以这里
    // 先落一份 owning 的表再借出键 —— 不拿 `Box::leak` 糊过去：刷新是长期
    // 后台动作，每次泄漏十几条字符串不是"小开销"，是漏。
    let prepared: Vec<(String, String)> = solo_headers(&identity, false).into_iter().collect();
    let headers: Vec<(&str, String)> = prepared.iter().map(|(name, value)| (name.as_str(), value.clone())).collect();
    let reply: Reply = match post_json(&catalog_url(), &catalog_body(credential.variant()), &headers, std::time::Duration::from_secs(20), proxy).await {
        Ok(reply) => reply,
        Err(error) => return ModelRefreshOutcome::failed(format!("目录请求失败：{}", error.message)),
    };
    if reply.status >= 400 {
        return ModelRefreshOutcome::failed(describe_failure(reply.status, &reply.body));
    }
    let Some(payload) = reply.json() else {
        return ModelRefreshOutcome::failed("上游目录响应不是合法 JSON".to_string());
    };
    let models = parse_catalog(&payload);
    if models.is_empty() {
        // 这条文案不能写成"上游没有模型"就完事：真实原因多半是
        // `X-Ide-Version-Code` 那一道版本闸门（版本不对时上游给的是**另一张表**
        // 或空表），或者是目录字段改名让解析整个落空。
        return ModelRefreshOutcome::failed(format!(
            "上游目录里没有可见模型（config_info_list 缺失/为空？IdeVersionCode={IDE_VERSION_CODE}）"
        ));
    }
    let count = models.len();
    let now = logging::now_ms();
    // 只换内存里那张表（持久化缓存不迁，见 `slot()` 的说明）。
    {
        let mut guard = slot().write().unwrap_or_else(|error| error.into_inner());
        guard.models = models;
        guard.fetched_at = now;
    }
    logging::log("[Models]", &format!("Trae 模型目录已刷新（{count} 个模型）"));
    ModelRefreshOutcome::refreshed(count)
}

/// 目录请求失败时给的一句**有指向性**的话（状态码 + 原文片段）。
fn describe_failure(status: u16, body: &str) -> String {
    let head: String = body.chars().take(120).collect();
    match status {
        401 | 403 => format!("上游返回 HTTP {status}（凭据可能已失效，请重新登录该账号）"),
        _ if head.is_empty() => format!("上游返回 HTTP {status}"),
        _ => format!("上游返回 HTTP {status}：{head}"),
    }
}

/// 这个客户端名是不是本家认识的（含带后缀与裸名两种写法）。
/// `#[allow(dead_code)]`：`payload::prepare_body` 自己判模型名，这里这份
/// 是给「网关侧校验客户端请求的模型名」那类调用点留的（插件协议里没有
/// 那个钩子 —— 网关不再认识具体 provider）。
#[allow(dead_code)]
pub fn is_known(model: &str) -> bool {
    let needle = model.trim();
    !needle.is_empty() && list().iter().any(|entry| text(entry, "id") == needle)
}

/// 客户端名 → 上游 config 名。
///
/// 两条都认：广告名 `<config>-solo` 与裸 config 名（后者是"用户照上游文档
/// 写名字"的常见情形，且 `sanitize_model_name` 对裸名本来就是恒等）。
/// `#[allow(dead_code)]`：与 `is_known` 同一处境（见那里的说明）。
#[allow(dead_code)]
pub fn upstream_name(model: &str) -> String {
    super::payload::sanitize_model_name(model, "solo")
}

