//! ZCode 的模型清单（**静态表**，两地共用一份）。
//!
//! ── 为什么是静态表而不是远程目录 ────────────────────────────
//! 上游没有「列模型」的公开接口：编码套餐的可用模型由**客户端内置的目录**
//! 决定，参考实现（`Acankao/zcode-api` 的 `src/provider/models.ts`）同样把它
//! 硬编码成常量表，并在注释里写明「新模型发布或规格变化时更新这张表」。
//!
//! 因此本模块不提供 `refresh`：`refresh_models` 会如实回答「这家没有远程目录」
//! （`supports_model_refresh() = false`），而不是假装刷了一次。
//! 这与 raccoon / autoclaw 那两家「有远程目录」的情形不同，别照抄它们的结构。
//!
//! ── 两地清单为什么逐字相同 ──────────────────────────────────
//! 参考实现的两处注记都写着 zai 与 bigmodel 的目录条目**完全一致**
//! （`models_catalog.json` 的 `zai` / `bigmodel` 两段同值）。差异在**账号能
//! 用到哪些**（套餐档位决定），不在清单本身 —— 而「某账号能用哪些」是上游
//! 按套餐判的（用不到的模型会回 `502 exceed quota limit`），本机无法预先枚举。
//! 所以两地共用一份清单，把「用不到」留给上游如实报错，与参考实现一致。
//!
//! ── 字段形状 ────────────────────────────────────────────────
//! 条目用的是**聚合目录认的键**（`id` / `name` / `maxInputTokens` /
//! `maxOutputTokens` / `supportsImages` / `supportsReasoning`），
//! 由 `models::shape::list_item` 翻成 OpenAI 形态 —— 与 raccoon 那家
//! 「上游原始形态 + 出口翻译」的做法不同：本家没有上游原始形态可言
//! （清单就是本机常量），所以直接按聚合层的输入契约书写，少一层无谓映射。
//!
//! ── `glm-5.3-flash` 为什么要广告出来 ────────────────────────
//! 它是**体验套餐（周末套餐）**被领到之后实际可用的那一档 —— 参考实现为此
//! 专门加了一条注记：「start-plan gateway serves the -flash variant (used by
//! claimed trial plans like the weekend package); advertised so client-side
//! discovery lists it」。不广告它，用户领到套餐后在 `/v1/models` 里看不到
//! 能用的模型，会以为领取失败。

use serde_json::{json, Value};

use super::region::Region;

/// 一个模型的静态规格。
///
/// ── 为什么只有三个字段（2026-10 收窄）─────────────────────────
/// 这里原来是「上下文 / 输出上限 / 是否思考 / 是否视觉」四列，全部转录自参考实现
/// 的常量表。它们既不来自上游接口、也没有逐条实测，而且已经能被证伪（models.dev
/// 的实验室自述：`glm-5.1` 的输出上限是 131K 而我们标 64K，`glm-5` 的窗口是
/// 204800 而我们标 200000）。**没有依据的声明比空着更糟** —— 下游会照它构造请求
/// （按虚高的窗口堆历史）或据此拒绝能力，而界面上看不出这是猜的。
///
/// 所以：能力位与上下限一律**不在这里声明**，由 `core::model_meta` 从 models.dev
/// 补齐（那份对本家每一支模型都有收录）；只有下面这两个字段是本模块真正要用的 ——
/// `max_output` 服务思考预算截顶（`zcode::reasoning`，内部安全上限），
/// `reasoning` 只给**目录明确写过**的两档视觉模型（见 `MODELS` 的注释）。
struct Spec {
    /// 上游模型 id（**原样转发**，不做任何改名）
    id: &'static str,
    /// 展示名
    name: &'static str,
    /// 最大输出（**内部**用：思考预算截顶的上限，不直接出口）
    max_output: i64,
    /// 是否把「支持思考」**声明给下游**。`None` = 不声明（交给 models.dev）。
    reasoning: Option<bool>,
}

/// 编码套餐的模型表（id / 展示名 / 内部输出上限，见 `Spec` 的说明）。
///
/// 顺序 = 展示顺序：能力由强到弱、同代相邻，便于管理页阅读。
const MODELS: &[Spec] = &[
    Spec { id: "glm-5.3", name: "GLM-5.3", max_output: 128_000, reasoning: None },
    // `glm-5.3-flash` 的视觉位由 models.dev 给出（目录 `agent/configs` 的
    // `capabilities.vision` 与实测一致：2026-09-30 发一张红色 4×4 PNG 它答「红色」；
    // 同一张图打 `glm-5.3` 被 400 拒掉，原文是「messages.content.type 参数非法，
    // 取值范围 ['text']」）。
    Spec { id: "glm-5.3-flash", name: "GLM-5.3 Flash", max_output: 128_000, reasoning: None },
    Spec { id: "glm-5.2", name: "GLM-5.2", max_output: 128_000, reasoning: None },
    Spec { id: "glm-5.1", name: "GLM-5.1", max_output: 64_000, reasoning: None },
    Spec { id: "glm-5", name: "GLM-5", max_output: 64_000, reasoning: None },
    Spec { id: "glm-5-turbo", name: "GLM-5 Turbo", max_output: 64_000, reasoning: None },
    Spec { id: "glm-4.7", name: "GLM-4.7", max_output: 131_072, reasoning: None },
    Spec { id: "glm-4.6", name: "GLM-4.6", max_output: 131_072, reasoning: None },
    Spec { id: "glm-4.5-air", name: "GLM-4.5 Air", max_output: 98_304, reasoning: None },
    // 视觉两档**反对**思考，这两行是本表唯一保留 `reasoning` 的地方：
    // 目录里明确写着它们不支持思考，且标错的方向是不对称的 —— 声明成 true 会让
    // 客户端按「支持思考」去发 `thinking`，上游直接拒（400）；声明成 false 只是
    // 让客户端不启用思考。models.dev 对这两支给的是 true（那是实验室 API 的口径，
    // 与本通道的目录不一致），按「上游声明 > models.dev」取本目录的 false。
    Spec { id: "glm-4.6v", name: "GLM-4.6V", max_output: 32_768, reasoning: Some(false) },
    Spec { id: "glm-5v-turbo", name: "GLM-5V Turbo", max_output: 131_072, reasoning: Some(false) },
];

/// 一个模型的**目录补充信息**（`agent/configs` 的 `builtinModels[]`）
struct Extra {
    /// 上游模型 id（与本表的 `Spec::id` 同一口径：小写）
    id: &'static str,
    /// 视频输入。`None` = 未声明（出口不出这个键，管理页显示「未声明」）
    video: Option<bool>,
    /// 可配的思考档位（出口为 `reasoningLevels`；空 = 未声明）
    levels: &'static [&'static str],
    /// 默认思考档位（出口为 `reasoningDefaultLevel`）
    default_level: &'static str,
}

/// 目录补充信息表。
///
/// ── 为什么与 [`MODELS`] 分开放 ───────────────────────────────
/// 这两项只有**有依据的家族**才在表里：5.3 是目录 + 实测，5.2 是智谱开放文档
/// 给出的档位兼容映射（见下）。塞进 `Spec` 会让其它几行各拖一串「没有」的
/// 占位值，反而看不出「谁真有数据」；分表之后，没有依据的模型就是**不在表里**，
/// 出口也如实不出这些键（管理页显示「未声明」，见 `ui-islands` 的
/// `model-capability.ts` 三态说明）。
///
/// ── 档位值的来源 ────────────────────────────────────────────
/// 写的是**上游语义上的目标档位**（与本家 `reasoning.rs` 归一后的发出口径
/// 逐字对应）：
///   · 5.3：目录给每个档位写「往 `output_config.effort` 塞什么值」，三个档位
///     各一条（`low` / `high` / `max`），`defaultLevel` 是 `max`；
///   · 5.2：智谱开放文档的兼容映射（`none` / `minimal` 放弃思考、`low` /
///     `medium` → `high`、`xhigh` → `max`、默认 `max`）——「能落到哪一档」的
///     集合是 `minimal` / `high` / `max`，与 `reasoning.rs` 的
///     `normalize_glm52` 一致。
const CATALOG_EXTRAS: &[Extra] = &[
    Extra {
        id: "glm-5.3",
        video: Some(false),
        levels: &["low", "high", "max"],
        default_level: "max",
    },
    Extra {
        id: "glm-5.3-flash",
        video: Some(true),
        levels: &["low", "high", "max"],
        default_level: "max",
    },
    // 5.2 的视频输入能力没有依据（目录里没有它的条目），如实不出 supportsVideo
    Extra {
        id: "glm-5.2",
        video: None,
        levels: &["minimal", "high", "max"],
        default_level: "max",
    },
];

/// 本家的模型清单（聚合目录认的形态）。
///
/// ── 只声明**有据可依**的键（2026-10 起）────────────────────────
/// 出口只留两类：
///   · **协议级事实**：`supportsToolCall`（编码套餐的客户端本身就是靠工具调用跑
///     agent 循环）；两档视觉模型的 `supportsReasoning: false`（目录明确写过，
///     见 `MODELS` 的注释）；
///   · `CATALOG_EXTRAS` 里**有注记**的项（5.3 家族的档位来自官方目录、5.2 来自
///     智谱公开文档、视频位逐条注明是否有依据）。
/// 其余（上下文 / 输出上限 / 图片 / 思考）由 `core::model_meta` 从 models.dev
/// 补齐 —— 那一份对本家的每一支模型都收录（见同步脚本产出的快照），
/// 而原先转录自参考实现的那些值已被证伪（理由见 `Spec` 的说明）。
///
/// **规格表本身保留**：`models::max_output` 仍是本家思考预算截顶的内部上限
/// （见 `zcode::reasoning`）—— 那是「别让 max_tokens 超过上游能返的量」的安全
/// 兜底，与对下游声明什么无关；保守值的代价只是截顶更早，方向安全。
///
/// `region` 当前**不影响**返回值（两地清单相同，见模块头），参数保留是为了
/// 让调用点写成 `models::list(region)` —— 将来若真出现地区差异（例如某地上了
/// 独家模型），改这一处即可，不必回头改所有调用点。
pub fn list(_region: Region) -> Vec<Value> {
    MODELS
        .iter()
        .map(|spec| {
            let mut item = json!({
                "id": spec.id,
                "name": spec.name,
                "supportsToolCall": true,
            });
            if let Some(reasoning) = spec.reasoning {
                item["supportsReasoning"] = Value::Bool(reasoning);
            }
            if let Some(extra) = CATALOG_EXTRAS.iter().find(|extra| extra.id == spec.id) {
                append_catalog_extras(&mut item, extra);
            }
            item
        })
        .collect()
}

/// 把目录补充信息写进一条清单项（只写**有依据**的键）。
///
/// 没有依据的模型这几项在出口里**缺失**，管理页照三态显示成「未声明」——
/// 而不是被我们替上游答一个「不支持」（两者对用户是两件事，见
/// `ui-islands` 的 `model-capability.ts`）。
fn append_catalog_extras(item: &mut Value, extra: &Extra) {
    let Some(object) = item.as_object_mut() else {
        return;
    };
    if let Some(video) = extra.video {
        object.insert("supportsVideo".to_string(), Value::Bool(video));
    }
    if extra.levels.is_empty() {
        return;
    }
    object.insert(
        "reasoningLevels".to_string(),
        Value::Array(
            extra
                .levels
                .iter()
                .map(|level| Value::String((*level).to_string()))
                .collect(),
        ),
    );
    object.insert(
        "reasoningDefaultLevel".to_string(),
        Value::String(extra.default_level.to_string()),
    );
}

/// 这个模型名是不是本家认识的（**广告视图**的判据，见 `adapter` 的 `list_models`）。
///
/// 大小写不敏感：上游模型 id 是小写，而客户端偶尔会带上原始大小写
/// （`GLM-5.3`），若按严格相等判定会让「列表里明明有」的名字点不动。
#[allow(dead_code)]
// `is_known` 是网关侧「模型名认不认」的判据（模型管理页用），插件没有调用方。
// 保留：它是 `max_output` 那一族的公开入口，删了会让 `models` 只剩一张表。
pub fn is_known(model: &str) -> bool {
    find(model).is_some()
}

/// 模型的最大输出额度（思考预算相加后的**截顶**用，见 `zcode::reasoning`）。
///
/// 与 [`is_known`] 同源的查表（同一份大小写不敏感的比较），`None` = 不认识这个
/// 模型 —— 调用方据此放弃截顶而不是拿一个假上限去截。
pub(super) fn max_output(model: &str) -> Option<i64> {
    find(model).map(|spec| spec.max_output)
}

/// 按 id 找规格（大小写不敏感；`find` 是本文件唯一的比较口径，避免两处漂移）
fn find(model: &str) -> Option<&'static Spec> {
    let needle = model.trim().to_ascii_lowercase();
    MODELS.iter().find(|spec| spec.id == needle)
}
