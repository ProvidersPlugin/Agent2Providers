//! Trae 的额度与用量读数（`ide_user_ent_usage` + `ide_user_pay_status`）。
//!
//! ── 为什么一次查询要读两个接口 ──────────────────────────────
//! 上游把"你还剩多少"拆在两份**互相不知道对方存在**的账里：
//!   * `ide_user_ent_usage` 给权益包（entitlement pack）清单 —— 订阅制账户的
//!     `basic_usage_limit`、速通的 `premium_model_fast_request_limit`、
//!     以及 SOLO 积分制真正扣减的 `credits_limit` 都在这里；
//!   * `ide_user_pay_status` 给套餐 detail/quota —— **Free/SOLO 账户的 ent_usage
//!     包里根本没有可解析的 quota**，「快请求/月」「SOLO 并发」这些维度只在
//!     这一份里。参考实现 v0.12.29 加它就是因为只读 ent_usage 时这类账户整列显示 `--`。
//!
//! 第二条是 best-effort：它失败不阻塞积分读数（与参考实现同一取向 ——
//! 这是展示动作，为了附加维度把整格变红是得不偿失）。
//!
//! ── 聚合读数之外还有一份逐包明细 ────────────────────────────
//! `document()` 出的 `wallets` 回答「这个账号还剩多少」，而钱具体在哪个包里、
//! 哪一份什么时候作废，只有**清单**答得上来 —— 那就是 `credits[]`
//! （一行一份可见权益包，带领取 → 到期，见 [`credit_rows`]）。上游的
//! `start_time` / `expire_time` 两个时间字段是这份明细的由来：本模块此前只读
//! `end_time`（套餐周期边界），而积分包里真正决定"这笔钱什么时候没了"的是
//! `expire_time`，两者在免费包里能差好几年。
//!
//! ── 三条不能"顺手美化"的读数纪律（全部由向量钉住）──────────────
//!   1. **缺失 ≠ 0**。quota 里每个数值都是"上游没给"与"给了 0"两回事，
//!      参考实现把它写成 `*int64` 就是这个原因；它修过的一个真实 bug 正是
//!      "读一个不存在的 `credits_limit`，把 Free/SOLO 账户渲染成剩余 0 积分"。
//!      本模块因此用 `Option<i64>`，聚合结果的 `remain_known` 为 false 时
//!      界面上是「—」而不是 0（`RemainKnown` 在参考实现里就叫这个名字）。
//!   2. **`-1` 是"不限"，不是"耗尽"**。速通与积分池都拿 -1 表达无上限，
//!      聚合时 `available` 直接给 -1 并停止累加。数值列画成 `-1` 会被读成
//!      "欠费"，所以 `available` 只在拿到有限数时才给数，"不限"走展示串。
//!   3. **bonus 只加在可见包上**，且主额度超支时正 bonus 仍要加回来
//!      （`basic_limit 100 / 已用 120 / bonus 50 用了 10` → 剩 20）。
//!
//! ── 为什么头与转发用的不是一套 ──────────────────────────────
//! ug 族（积分/权益）走的是**另一条画像**：VSCode 插件进程 UA、
//! `Package-Type`、`X-User-Region`、`Sec-Fetch-*`，而**没有** `X-Uid`、
//! `X-Machine-Id`、`X-Cloudide-Token`/`X-Ide-Token`（对话那三头一份都不带）。
//! 域名也不同：对话是 `trae-api-cn.mchost.guru`，这一族固定
//! `https://api.trae.cn`（参考实现的包级常量，**不看账号 apiHost**）。
//! 把 SOLO 头照搬过来的后果不是报错，而是 401 `code=1001`
//! 「we are not able to authenticate you」—— 谱系/画像不匹配时上游就是这么回。
//!
//! ── 与参考实现的两处**故意**不同 ────────────────────────────
//!   * 数值宽容：Go 用 `*int64` 反序列化，上游一旦把 `basic_usage_limit`
//!     写成 `100.0` 或字符串，**整份响应解析失败**；这里按 `Option<i64>`
//!     取整收下，收不下才当缺失。余额是只读展示动作，不该因为一个小数点整格变红。
//!   * 不做签到：`checkin_credits/status` 那条链在参考实现里有一段把出口 IP
//!     打进封禁的前科（9074 重试链 + 面板连点），而且它记的是**签到钱包**，
//!     与模型调用真正扣的积分池是两笔钱。本模块因此完全不碰它。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap / expect / panic；持锁期间不做网络。

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use agent2api_provider_sdk::PluginCtx;
use agent2api_provider_protocol::WireProxy as ResolvedProxy;
use crate::errors::GatewayError;

use super::adapter::{account_proxy, read_record, renew_if_due};
use super::credentials::Credential;
use super::errors::classify;
use super::headers::{DEVICE_BRAND, IDE_VERSION, OS_VERSION};
use super::http::{Reply, post_json};

/// 积分/权益那条链的域名（参考实现的包级常量 `UgHost`，与账号 `apiHost` 无关）。
pub const UG_HOST: &str = "https://api.trae.cn";
/// 权益包清单（主读数）。
pub const EP_ENT_USAGE: &str = "/trae/api/v2/pay/ide_user_ent_usage";
/// 套餐 detail/quota（Free/SOLO 的唯一 quota 来源，best-effort）。
pub const EP_PAY_STATUS: &str = "/trae/api/v1/pay/ide_user_pay_status";

/// 余额接口的总超时。必须显式设：`egress` 默认的 read_timeout 是 600 秒
/// （留给 SSE 长连接），不设的话一个挂住的余额查询会把批量查询拖到一直转圈。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// 错误体进消息的截断长度（与参考实现的 200 字符同一用意：别把 HTML 整页灌进日志）。
const ERROR_BODY_HEAD: usize = 200;

/// 用量口径（参考实现 `UsageModel` 的三个值，逐字一致以便对拍）。
pub const MODEL_FAST: &str = "fast";
pub const MODEL_BASIC: &str = "basic";
pub const MODEL_UNKNOWN: &str = "unknown";

/// `-1`（不限）在展示里的样子。
const UNLIMITED_VIEW: &str = "不限";

// ── 解析层：上游 JSON → 权益包 ────────────────────────────────

/// quota 层的四个已知数值字段。`None` = 上游没给这个键（见模块头纪律 1）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PackQuota {
    pub basic_usage_limit: Option<i64>,
    pub bonus_usage_limit: Option<i64>,
    pub fast_request_limit: Option<i64>,
    pub credits_limit: Option<i64>,
    /// SOLO 并发数（`solo_agent_parallel_limit`）。**元数据**：不参与
    /// `has_any()` 的三层探测，也不参与任何求和（见文件末尾那条增补说明）。
    pub solo_parallel: Option<i64>,
    /// 这一层里 `enable_solo_*` 是否有任一为 true。同上，纯元数据。
    pub solo_enabled: bool,
}

impl PackQuota {
    /// 这一层有没有**任何**已知字段。`EffectiveQuota` 的三层探测靠它决定
    /// 往下走一层，还是就地收 —— 空对象 `{}` 也算"没给"（向量里那条 `quota:{}`
    /// 的用例走的就是 subscription_extra）。
    fn has_any(&self) -> bool {
        self.basic_usage_limit.is_some()
            || self.bonus_usage_limit.is_some()
            || self.fast_request_limit.is_some()
            || self.credits_limit.is_some()
    }
}

/// `pack.usage` 的已知数值字段。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PackUsage {
    pub basic_amount: Option<i64>,
    pub bonus_amount: Option<i64>,
    pub fast_amount: Option<i64>,
    /// 积分池已用量：上游给的是**浮点**，官方 cashier 先减再 `Math.round`。
    pub credits_amount: Option<f64>,
}

/// 一个权益包（`user_entitlement_pack_list` 的一条）。
#[derive(Clone, Debug, Default)]
pub struct Pack {
    /// 0=Free 1=Pro 4=Pro+ 5=Pro+(CN 别名) 6=Ultra 8=Lite 9=Trial 100=CNExpress 3=PROMO_CODE
    pub product_type: i64,
    /// 套餐到期时间，**秒**级 epoch（本模块只把它换算成毫秒给界面，不参与额度判定）。
    pub end_time: i64,
    /// 这份包的**开始/发放**时刻（上游 `entitlement_base_info.start_time`）——
    /// 界面明细里「领取 …」那一半。同样只做展示，不参与任何判定。
    ///
    /// 取它而不是拿 `DeductionStartTime` 那类近似值顶替：每日签到发的包在这里
    /// 是**发放当刻**，而周期字段只是"这一天/这个月的起点"，差着一整笔的来源。
    pub start_time: i64,
    /// 顶层 `expire_time`：这份包**作废**的时刻。0 = 上游没给。
    ///
    /// 与 `end_time` 不是一回事（见 [`credit_rows`] 的口径说明）：`end_time` 是
    /// 订阅周期的边界，免费包里能比它晚出去好几年。
    pub expire_time: i64,
    pub is_hide: bool,
    /// `None` = 缺省（视为 active）。`Some(3)` = 已取消。
    pub status: Option<i64>,
    pub base_quota: PackQuota,
    pub subscription_quota: PackQuota,
    pub package_quota: PackQuota,
    pub usage: PackUsage,
    /// 上游给的人类可读套餐名，优先于 `product_type` 映射。
    pub display_desc: String,
}

/// 读 `is_credits_billing`（决定"没有任何 credits_limit 时要不要按 0 展示"）。
pub fn is_credits_billing(payload: &Value) -> bool {
    payload.get("is_credits_billing").and_then(Value::as_bool).unwrap_or(false)
}

/// `user_entitlement_pack_list` → 包列表（顺序保留上游给的顺序：`SelectActivePack`
/// 的兜底分支就是"取第一条"，顺序一变选中的包就变了）。
pub fn parse_packs(payload: &Value) -> Vec<Pack> {
    let entries = match payload.get("user_entitlement_pack_list").and_then(Value::as_array) {
        Some(list) => list,
        None => return Vec::new(),
    };
    entries.iter().map(pack_of).collect()
}

fn pack_of(entry: &Value) -> Pack {
    let base = entry.get("entitlement_base_info");
    let extra = base.and_then(|info| info.get("product_extra"));
    Pack {
        product_type: number(base.and_then(|info| info.get("product_type"))).unwrap_or(0),
        end_time: number(base.and_then(|info| info.get("end_time"))).unwrap_or(0),
        // 这两个只给明细排版用，缺了就是 0（"上游没给"，不是"时刻是 0"）
        start_time: number(base.and_then(|info| info.get("start_time"))).unwrap_or(0),
        expire_time: number(entry.get("expire_time")).unwrap_or(0),
        is_hide: base.and_then(|info| info.get("is_hide")).and_then(Value::as_bool).unwrap_or(false),
        // 缺省与 null 都是 None（Go 的 `*int` 同义），判反会把整张清单剔空
        status: number(base.and_then(|info| info.get("status"))),
        base_quota: quota_of(base.and_then(|info| info.get("quota"))),
        subscription_quota: quota_of(extra.and_then(|extra| extra.get("subscription_extra")).and_then(|part| part.get("quota"))),
        package_quota: quota_of(extra.and_then(|extra| extra.get("package_extra")).and_then(|part| part.get("quota"))),
        usage: usage_of(entry.get("usage")),
        display_desc: entry.get("display_desc").and_then(Value::as_str).unwrap_or("").trim().to_string(),
    }
}

fn quota_of(value: Option<&Value>) -> PackQuota {
    let Some(value) = value else {
        return PackQuota::default();
    };
    PackQuota {
        basic_usage_limit: number(value.get("basic_usage_limit")),
        bonus_usage_limit: number(value.get("bonus_usage_limit")),
        fast_request_limit: number(value.get("premium_model_fast_request_limit")),
        credits_limit: number(value.get("credits_limit")),
        solo_parallel: number(value.get("solo_agent_parallel_limit")),
        solo_enabled: SOLO_FLAG_KEYS.iter().any(|key| value.get(*key).and_then(Value::as_bool) == Some(true)),
    }
}

/// 上游可能把 SOLO 标志放在**任意一层** quota 里（实测免费包同时出现在
/// `entitlement_base_info.quota` 与 `product_extra.subscription_extra.quota`）。
const SOLO_FLAG_KEYS: [&str; 5] = ["enable_solo_agent", "enable_solo_builder", "enable_solo_coder", "enable_solo_lite", "enable_solo_web"];

fn usage_of(value: Option<&Value>) -> PackUsage {
    let Some(value) = value else {
        return PackUsage::default();
    };
    PackUsage {
        basic_amount: number(value.get("basic_usage_amount")),
        bonus_amount: number(value.get("bonus_usage_amount")),
        fast_amount: number(value.get("premium_model_fast_amount")),
        credits_amount: value.get("credits_amount").and_then(Value::as_f64),
    }
}

fn number(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    value.as_i64().or_else(|| value.as_f64().map(|number| number as i64))
}

// ── 判定层：纯函数，全部由向量 `usage` / `payStatus` 段钉住 ──────

/// 三层 quota 探测（参考实现 `EffectiveQuota`）：
/// `entitlement_base_info.quota` → `product_extra.subscription_extra.quota`
/// → `product_extra.package_extra.quota`，取**第一层带任何已知字段**的。
pub fn effective_quota(pack: &Pack) -> &PackQuota {
    if pack.base_quota.has_any() {
        return &pack.base_quota;
    }
    if pack.subscription_quota.has_any() {
        return &pack.subscription_quota;
    }
    &pack.package_quota
}

/// 这个包参不参与聚合（参考实现三处重复的同一条过滤，合并成一处）。
/// `product_type == 3` 是 PROMO_CODE（兑换码活动包），上游明确排除。
fn is_active_pack(pack: &Pack) -> bool {
    pack.product_type != 3 && !pack.is_hide && pack.status != Some(3)
}

fn active_packs(packs: &[Pack]) -> Vec<&Pack> {
    packs.iter().filter(|pack| is_active_pack(pack)).collect()
}

/// 一个包的剩余额度（`None` = quota 缺失，"剩余未知"，**不是 0**）。
pub fn pack_remain(pack: &Pack) -> Option<i64> {
    let quota = effective_quota(pack);
    let limit = quota.basic_usage_limit?;
    let mut left = limit - pack.usage.basic_amount.unwrap_or(0);
    if let Some(bonus_limit) = quota.bonus_usage_limit {
        let bonus_left = bonus_limit - pack.usage.bonus_amount.unwrap_or(0);
        if bonus_left > 0 {
            left += bonus_left;
        }
    }
    Some(left.max(0))
}

/// 速通（fast request）口径的合计。
#[derive(Clone, Debug, PartialEq)]
pub struct FastUsage {
    pub available: i64,
    pub limit: i64,
    pub used: i64,
    /// 任一包给了 `-1` → 整体不限（此时 `available`/`limit` 都是 -1）。
    pub unlimited: bool,
}

/// 对可见包求 `premium_model_fast_request_limit` 之和与
/// `premium_model_fast_amount` 之和；`available = limit - used`。
///
/// 返回 `None` 表示**这些包里一个 fast 字段都没有**（"没有证据"），
/// 与"证据是 0"是两回事 —— 把前者当后者就是那个"免费账户显示剩 0 次"的 bug。
///
/// `dashboard_payload` 是参考实现给另一个数据源（`user_current_entitlement_list`）
/// 用的：那边"包存在"本身就算证据。本模块唯一的调用点传 `false`。
pub fn fast_request_usage(packs: &[Pack], dashboard_payload: bool) -> Option<FastUsage> {
    let filtered = active_packs(packs);
    if filtered.is_empty() {
        return None;
    }
    let mut has_evidence = dashboard_payload;
    let mut limit = 0i64;
    let mut used = 0i64;
    let mut unlimited = false;
    for pack in &filtered {
        let quota = effective_quota(pack);
        if let Some(value) = quota.fast_request_limit {
            has_evidence = true;
            if value == -1 {
                unlimited = true;
            } else {
                limit += value;
            }
        }
        if let Some(value) = pack.usage.fast_amount {
            has_evidence = true;
            used += value;
        }
    }
    if !has_evidence {
        return None;
    }
    if unlimited {
        return Some(FastUsage { available: -1, limit: -1, used, unlimited: true });
    }
    Some(FastUsage { available: (limit - used).max(0), limit, used, unlimited: false })
}

/// CN 与 Intl 的优先级**不同表**（CN 多 100 与 5 两档）。抄成一张表的话，
/// 同一份包在两个谱系下会选中不同的包、读出不同的余额（向量里那条
/// "国际版优先级没有 100/5" 就是为了区分这两张表）。
const PRIORITY_CN: [i64; 8] = [100, 6, 5, 4, 1, 9, 8, 0];
const PRIORITY_INTL: [i64; 6] = [6, 4, 1, 9, 8, 0];

/// 选中"当前生效"的那个包（先看优先级表，全不在表里时取过滤后的第一条）。
pub fn select_active_pack(packs: &[Pack], is_cn: bool) -> Option<&Pack> {
    let filtered = active_packs(packs);
    if filtered.is_empty() {
        return None;
    }
    let order: &[i64] = if is_cn { &PRIORITY_CN } else { &PRIORITY_INTL };
    for product_type in order {
        for pack in &filtered {
            if pack.product_type == *product_type {
                return Some(pack);
            }
        }
    }
    filtered.first().copied()
}

/// SOLO 积分池（官方 cashier 同口径）—— 模型调用真正扣减的那笔钱。
#[derive(Clone, Debug, PartialEq)]
pub struct CreditsPool {
    pub remain: i64,
    pub known: bool,
    pub unlimited: bool,
}

/// `Σ max(credits_limit - credits_amount, 0)`，逐包 `Math.round`；
/// 任一包 `credits_limit == -1` → 整体不限。
///
/// `is_credits_billing = true` 时即使没有任何 `credits_limit` 也算"已知"
/// （官方此时按 0 展示）；false 且没有字段 → `known = false`（界面显示 `--`）。
pub fn credits_pool_usage(packs: &[Pack], is_credits_billing: bool) -> CreditsPool {
    let filtered = active_packs(packs);
    let has_field = filtered.iter().any(|pack| effective_quota(pack).credits_limit.is_some());
    if !is_credits_billing && !has_field {
        return CreditsPool { remain: 0, known: false, unlimited: false };
    }
    if filtered.is_empty() {
        return CreditsPool { remain: 0, known: is_credits_billing, unlimited: false };
    }
    let mut unlimited = false;
    let mut total = 0i64;
    for pack in &filtered {
        let Some(limit) = effective_quota(pack).credits_limit else {
            continue; // 官方 `?? 0`：没有这个键就贡献 0
        };
        if limit == -1 {
            unlimited = true;
            continue;
        }
        let used = pack.usage.credits_amount.unwrap_or(0.0);
        let left = (limit as f64 - used).max(0.0);
        total += (left + 0.5) as i64; // 官方 Math.round（left 已夹到非负，等价四舍五入）
    }
    if unlimited {
        return CreditsPool { remain: -1, known: true, unlimited: true };
    }
    CreditsPool { remain: total, known: true, unlimited: false }
}

/// 一次 `ent_usage` 的汇总读数（参考实现 `UsageSummary` 的等价物）。
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub model: &'static str,
    /// fast：速通可用次数（-1 不限）；basic：套餐剩余。
    pub remain: i64,
    pub remain_known: bool,
    pub fast_limit: i64,
    pub fast_used: i64,
    /// basic 口径的已用与额度池（界面上"已用 N / 共 M"）。
    pub used: i64,
    pub total: i64,
    /// basic 口径下选中那个包的到期时间（秒级 epoch，0 = 上游没给）。
    pub end_time: i64,
}

impl Summary {
    fn unknown() -> Self {
        Summary {
            model: MODEL_UNKNOWN,
            remain: 0,
            remain_known: false,
            fast_limit: 0,
            fast_used: 0,
            used: 0,
            total: 0,
            end_time: 0,
        }
    }
}

/// 按"速通优先（CN）→ 套餐余量 → 非 CN 的速通兜底 → 未知"的顺序定档。
///
/// 顺序本身就是语义：CN 账户只要有 fast 证据就不看 basic，因为速通次数是
/// 官方面板上那一列；反过来 Intl 没有速通的展示语义，所以 fast 只当兜底，
/// **但仍不丢弃证据**（参考实现那条注释特意留着这个分支）。
pub fn summarize(packs: &[Pack], is_cn: bool) -> Summary {
    let fast = fast_request_usage(packs, false);
    if is_cn {
        if let Some(fast) = &fast {
            return Summary {
                model: MODEL_FAST,
                remain: fast.available,
                remain_known: true,
                fast_limit: fast.limit,
                fast_used: fast.used,
                ..Summary::unknown()
            };
        }
    }
    let selected = select_active_pack(packs, is_cn);
    if let Some(selected) = selected {
        if let Some(remain) = pack_remain(selected) {
            let quota = effective_quota(selected);
            return Summary {
                model: MODEL_BASIC,
                remain,
                remain_known: true,
                fast_limit: 0,
                fast_used: 0,
                used: selected.usage.basic_amount.unwrap_or(0),
                total: quota.basic_usage_limit.unwrap_or(0),
                end_time: selected.end_time,
            };
        }
    }
    if !is_cn {
        if let Some(fast) = &fast {
            return Summary {
                model: MODEL_FAST,
                remain: fast.available,
                remain_known: true,
                fast_limit: fast.limit,
                fast_used: fast.used,
                ..Summary::unknown()
            };
        }
    }
    let mut fallback = Summary::unknown();
    if let Some(selected) = selected {
        fallback.end_time = selected.end_time;
    }
    fallback
}

/// 账号池打分用的剩余（`None` = 未知）。无限速通记一个大常数，
/// 让"不限"的账号在轮询里排在前面而不是被当成 0 次最后选。
/// `#[allow(dead_code)]`：网关那份用它给账号池打分（挑「剩得最多」的账号），
/// 插件里没有池化调度那一层 —— 选谁由网关按 `priority` 决定。
#[allow(dead_code)]
pub fn pack_list_remain(packs: &[Pack], is_cn: bool) -> Option<i64> {
    let summary = summarize(packs, is_cn);
    if !summary.remain_known {
        return None;
    }
    if summary.remain < 0 {
        return Some(1 << 30);
    }
    Some(summary.remain)
}

/// `product_type` → 人类可读计划名（参考实现同表；5 在两地都是 `Pro+`，
/// 100 只有 CN 才是 `CNExpress`）。
pub fn product_type_identity(product_type: i64, is_cn: bool) -> &'static str {
    match product_type {
        100 => {
            if is_cn {
                "CNExpress"
            } else {
                "Unknown"
            }
        }
        6 => "Ultra",
        5 => "Pro+",
        4 => "Pro+",
        1 | 9 => "Pro",
        8 => "Lite",
        0 => "Free",
        _ => "Unknown",
    }
}

/// 套餐名：选中包的 `display_desc` 优先，其次 `product_type` 映射，都没有则 `Unknown`。
pub fn plan_name(packs: &[Pack], is_cn: bool) -> String {
    match select_active_pack(packs, is_cn) {
        Some(pack) if !pack.display_desc.is_empty() => pack.display_desc.clone(),
        Some(pack) => product_type_identity(pack.product_type, is_cn).to_string(),
        None => "Unknown".to_string(),
    }
}

/// 免费档判定（参考实现 `IsFreePlan`，但**补上中文"免费"** ——
/// 上游 `.includes('free')` 对 CN 的显示名"免费"会漏判）。
/// 匹配前都先 `trim` + 小写；子串匹配，所以"标准 Free 版"也算免费档。
pub fn is_free_plan(plan: &str, plan_type: &str) -> bool {
    let plan = plan.trim().to_lowercase();
    let plan_type = plan_type.trim().to_lowercase();
    plan.contains("free") || plan.contains("免费") || plan_type.contains("free") || plan_type.contains("免费")
}

// ── pay_status：detail/quota 的分层探测 ────────────────────────

/// `ide_user_pay_status` 的取值（参考实现的 `pickInt`）。
///
/// 回退层次**不对称**，这是本模块最容易写反的一处：
///   * detail：顶层 `detail` → `entitlementInfo.detail` → `originPayStatusData.detail`
///   * quota：顶层 `quota` → `entitlementInfo.quota`（**没有** originPayStatusData 这层）
///
/// 键名也有两套写法（`fast_request_per` / `fastRequestPer`）。
/// 键存在但不是数字 → 跳过、继续找下一键下一层（与参考实现一致：
/// 那里的 `json.Unmarshal` 失败不 return）。
pub fn pick_int(payload: &Value, quota: bool, keys: &[&str]) -> Option<i64> {
    let layers: Vec<&Value> = if quota {
        [payload.get("quota"), payload.get("entitlementInfo").and_then(|info| info.get("quota"))]
            .into_iter()
            .flatten()
            .collect()
    } else {
        [
            payload.get("detail"),
            payload.get("entitlementInfo").and_then(|info| info.get("detail")),
            payload.get("originPayStatusData").and_then(|origin| origin.get("detail")),
        ]
        .into_iter()
        .flatten()
        .collect()
    };
    for layer in layers {
        for key in keys {
            if let Some(value) = layer.get(*key) {
                if let Some(number) = number(Some(value)) {
                    return Some(number);
                }
            }
        }
    }
    None
}

/// 业务码（参考实现只在 `code == 0` 时采纳 pay_status 的维度；缺键按 0 处理
/// —— 上游这份响应本来就没有把 code 当必填）。
pub fn pay_code(payload: &Value) -> i64 {
    number(payload.get("code")).unwrap_or(0)
}

/// 快请求配额（detail.fast_request_per / fastRequestPer）。
pub fn fast_request_per(payload: &Value) -> Option<i64> {
    pick_int(payload, false, &["fast_request_per", "fastRequestPer"])
}

/// 速通状态位（detail.can_get_express_status / canGetExpressStatus）。
/// `#[allow(dead_code)]`：速通状态位的读数，当前面板不渲染它
/// （`summarize` 已经用 `premium_model_fast_request_limit` 表达了同一件事）。
#[allow(dead_code)]
pub fn can_get_express_status(payload: &Value) -> Option<i64> {
    pick_int(payload, false, &["can_get_express_status", "canGetExpressStatus"])
}

/// SOLO 并发数（quota.solo_agent_parallel_limit）。
pub fn solo_parallel_limit(payload: &Value) -> Option<i64> {
    pick_int(payload, true, &["solo_agent_parallel_limit"])
}

/// 有没有 SOLO 套餐：`quota.enable_solo_*` 任一为 `true`。
///
/// 只认 JSON 布尔：上游那侧是 `json.Unmarshal(raw, &bool)`，`1` 会解析失败
/// 当成"没有"。这里同样只收 `Value::is_true()`（向量钉住了 `enable_solo_agent:1`
/// 不算 SOLO 包这条）。
pub fn has_solo_package(payload: &Value) -> bool {
    let keys = ["enable_solo_agent", "enable_solo_builder", "enable_solo_coder", "enable_solo_lite", "enable_solo_web"];
    let layers = [payload.get("quota"), payload.get("entitlementInfo").and_then(|info| info.get("quota"))];
    for key in keys {
        for layer in layers.iter().flatten() {
            if layer.get(key).and_then(Value::as_bool) == Some(true) {
                return true;
            }
        }
    }
    false
}

/// 计划身份串（`user_pay_identity_str`，去空格；缺键 = 空串）。
pub fn plan_identity(payload: &Value) -> String {
    payload.get("user_pay_identity_str").and_then(Value::as_str).unwrap_or("").trim().to_string()
}

// ── SOLO 两个维度的**第二个来源**（本模块相对参考实现的一处有意增补）────
//
// 参考实现只从 `pay_status` 的 detail/quota 里取「SOLO 并发」与「有没有 SOLO 包」。
// 本机拿真实免费 SOLO 账号的响应对过一遍，结论是它对这类账号**取不到**：
//   * `pay_status` 顶层直接是 `enable_solo_builder/coder/lite/web`（没有 `quota` 包装），
//     `detail` 里只有 `fast_request_per` 这类；
//   * 而 `ide_user_ent_usage` 的包里，`quota` 就带着
//     `solo_agent_parallel_limit: 2` 与五个 `enable_solo_*: true`。
// 也就是说"这个账号能不能跑 SOLO、能跑几路"在 ent_usage 里是**现成的**，
// 只读 pay_status 会把它显示成空 —— 而这正是本模块要修的"整列 -- "那一类。
//
// 增补只做**兜底**：`pay_status` 给了就用它（与参考实现同口径，对拍不受影响），
// 没给才从包里的 quota 读。三个额度数值（remain / fast / 积分池）一个都没动：
// 这两个字段是**元数据**，不参与 `has_any()` 的三层探测，也不参与任何求和。
/// 从可见包的三层 quota 里找 `solo_agent_parallel_limit`（读到第一个即算数）。
pub fn solo_parallel_from_packs(packs: &[Pack]) -> Option<i64> {
    packs.iter().filter(|pack| is_active_pack(pack)).find_map(|pack| {
        pack.base_quota
            .solo_parallel
            .or(pack.subscription_quota.solo_parallel)
            .or(pack.package_quota.solo_parallel)
    })
}

/// 同上，找任一 `enable_solo_* = true`。
pub fn solo_package_from_packs(packs: &[Pack]) -> bool {
    packs.iter().filter(|pack| is_active_pack(pack)).any(|pack| {
        pack.base_quota.solo_enabled || pack.subscription_quota.solo_enabled || pack.package_quota.solo_enabled
    })
}

/// 上游给的汇总口径（`usage_summary`：总额、已用、消耗比）。
///
/// 这是**官方自己算的那一份**，与本模块按 `Σ max(limit-used,0)` 逐包取整算出的
/// 积分池是同一个池子的两种算法（逐包取整会差几分）。实测本机账号：
/// 我们 1468 / 官方 1550-81.77 = 1468.23 —— 差的是取整，不是漏读。
/// 因此它只作为**明细里的一行交叉核对**，不参与主读数（主读数与参考实现对齐）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OfficialSummary {
    pub total: f64,
    pub consumed: f64,
    pub ratio: Option<f64>,
}

pub fn official_summary(payload: &Value) -> Option<OfficialSummary> {
    let summary = payload.get("usage_summary")?;
    let total = summary.get("total_amount").and_then(Value::as_f64)?;
    let consumed = summary.get("consumed_amount").and_then(Value::as_f64)?;
    Some(OfficialSummary { total, consumed, ratio: summary.get("consumption_ratio").and_then(Value::as_f64) })
}

// ── 出站：ug 族的请求头与一次读取 ─────────────────────────────

/// 生成 uuid-v4 形状的请求标识（参考实现 `newRequestID`，抓包格式 8-4-4-4-12）。
fn request_id() -> String {
    let mut bytes = random_bytes::<16>();
    // 版本位与变体位（与参考实现同一处置：`(b[6] & 0x0f) | 0x40`、
    // `(b[8] & 0x3f) | 0x80`），少了这两处就不是 uuid-v4 的形状。
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = hex_lower(&bytes);
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// `X-TT-Trace-Id`（抓包格式 `00-<32hex>-<16hex>-01`，第二段取自请求 ID 去横线的前 16 位）。
///
/// 那个"取自 request id"的关系是抓包实证出来的，不是随便拼的第二段随机数 ——
/// 上游按它把同一请求的两条日志串起来，写断了这条链就断了。
fn trace_id(request_id: &str) -> String {
    let random = hex_lower(&random_bytes::<16>());
    let from_request = request_id.replace('-', "");
    let taken: String = from_request.chars().take(16).collect();
    let taken = if taken.is_empty() { "0000000000000000".to_string() } else { taken };
    format!("00-{random}-{taken}-01")
}

/// 随机字节；拿不到随机源时用**纳秒时间戳**兜底而不是 panic。
///
/// 这两个头只是客户端画像的一部分（每请求新生成），不是安全令牌。把它做成
/// 一个失败点，等于让"系统随机源不可用"这种本机故障变成一个查不出余额的理由。
fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    if getrandom::getrandom(&mut bytes).is_ok() {
        return bytes;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    for (index, slot) in bytes.iter_mut().enumerate() {
        *slot = ((nanos >> (8 * (index % 16))) & 0xff) as u8;
    }
    bytes
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// SOLO 之外的另一套画像：积分/权益（`api.trae.cn` 的 pay/usage 族）。
///
/// 与 `headers::solo_headers` 的三条差别都在向量里钉着：
///   * UA 是 **VSCode 插件进程**（`VSCode 1.107.1 (…)`），不是 IDE 主进程的 `Trae/0.1.61`；
///   * 令牌只进 `Authorization` 一个头（对话那边是三个头同值）；
///   * 没有 `X-Uid` / `X-Machine-Id`，但带 `Package-Type`、`X-User-Region`、
///     `X-Lgw-Req-Sdk-Type`、`Sec-Fetch-*` 与每请求新生成的两个 trace 头。
///
/// `X-User-Region: CN` 与 `Accept-Language: zh-CN` 是抓包值，参考实现对
/// intl 谱系也**照发 CN**（它自己注明"intl 维持存量行为，差异未验证"），
/// 这里同样不按 variant 分叉，免得把一处未验证的差异写成一处臆造。
pub fn ug_headers(variant: &str, access_token: &str, device_id: &str) -> BTreeMap<String, String> {
    let request = request_id();
    let mut headers = BTreeMap::new();
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    headers.insert("Accept".to_string(), "*/*".to_string());
    headers.insert("User-Agent".to_string(), format!("VSCode 1.107.1 ({})", platform_name_for(variant)));
    headers.insert("X-User-Region".to_string(), "CN".to_string());
    headers.insert("Accept-Language".to_string(), "zh-CN".to_string());
    headers.insert("Package-Type".to_string(), package_type_for(variant).to_string());
    headers.insert("X-Lgw-Req-Sdk-Type".to_string(), "3".to_string());
    headers.insert("X-Market-Client-Id".to_string(), "VSCode 1.107.1".to_string());
    headers.insert("X-Device-Brand".to_string(), DEVICE_BRAND.to_string());
    headers.insert("X-Device-Type".to_string(), "windows".to_string());
    headers.insert("X-OS-Version".to_string(), OS_VERSION.to_string());
    headers.insert("App-Version".to_string(), IDE_VERSION.to_string());
    headers.insert("X-Request-Id".to_string(), request.clone());
    headers.insert("X-TT-Trace-Id".to_string(), trace_id(&request));
    headers.insert("Sec-Fetch-Dest".to_string(), "empty".to_string());
    headers.insert("Sec-Fetch-Mode".to_string(), "no-cors".to_string());
    headers.insert("Sec-Fetch-Site".to_string(), "none".to_string());
    headers.insert("Authorization".to_string(), format!("Cloud-IDE-JWT {access_token}"));
    if !device_id.is_empty() {
        headers.insert("X-Device-Id".to_string(), device_id.to_string());
    }
    headers
}

/// 平台展示名（参考实现 `platformNameByVariant`，未知值兜到 `cn`）。
fn platform_name_for(variant: &str) -> &'static str {
    match variant {
        "solo" => "TRAE SOLO CN",
        "cn" => "TRAE CN",
        "intl" => "TRAE",
        "solo-intl" => "TRAE SOLO",
        _ => "TRAE CN",
    }
}

/// `Package-Type`：CN 谱系 `stable_cn`（抓包值），intl 谱系 `stable`。
fn package_type_for(variant: &str) -> &'static str {
    if variant == "intl" || variant == "solo-intl" {
        "stable"
    } else {
        "stable_cn"
    }
}

// ── 逐份明细（账号页「积分明细」弹层的输入）──────────────────

/// 秒/毫秒混给的 epoch → 毫秒。
///
/// 分界取 1e11（秒级到公元 5138 年也越不过它、毫秒级从 1973 年起就在它之上）——
/// 与 `document` 里 `expireAt` 的判据、与 workbuddy 资源包的 `epoch_to_millis`
/// 是同一个数，三处不能各写一个。
fn to_millis(value: i64) -> i64 {
    if value > 100_000_000_000 {
        value
    } else {
        value.saturating_mul(1000)
    }
}

/// 金额 → JSON：至多两位小数、整数不带小数点。
///
/// 积分池的数上游给的是浮点（`81.7668`），直接 `json!` 会打出四位小数；
/// 而 `1550.0` 这种整数形态在 serde 里会写成 `1550.0`（前端 `Number()` 读得动，
/// 但同一列里两种长度看起来像度量值）。整数输出整数形态，与 billing 那边
/// `json_number` 是同一取向。
fn amount_json(value: f64) -> Value {
    let rounded = (value * 100.0).round() / 100.0;
    if rounded.fract() == 0.0 {
        json!(rounded as i64)
    } else {
        json!(rounded)
    }
}

/// 逐份权益包的明细行（行形状与 workbuddy 的 `credit_packs_from` 逐键一致 ——
/// 界面因此只写一个读取器）。
///
/// ── 一行一份，不按名字归并 ──────────────────────────────────
/// 每日签到发的包会按天堆起来（社区实现实测能堆到 30 份同名包）。有实现按名字
/// 归并（面板放不下），但「哪一份什么时候到期」正是这个弹层要回答的问题，
/// 归并掉就等于把答案抹平 —— 明细的滚动交给界面（`.usage-pop` 有最大高度与
/// 滚动条），这里如实一份一行。
///
/// ── 三个口径都是「上游给什么写什么」─────────────────────────
///   * 额度**逐包选层**：有 `credits_limit` 的按积分（SOLO 积分制真正扣的那笔
///     钱），没有的退回套餐的 `basic_usage_limit`（订阅制账户只有这个）；
///     两层都没有的包仍然出一行（名字 + 到期时刻），但**一个数都不写**。
///   * `grantAt` ← `entitlement_base_info.start_time`（这份包开始/发放的时刻）、
///     `expireAt` ← 顶层 `expire_time`，没给才退回 `end_time`
///     （两者分工见 [`Pack::expire_time`] 的说明）。
///   * 缺失一律**不写这个键** —— 文件头第 1 条纪律：写成 0 会让界面把「没读到」
///     画成「只剩 0」。
///
/// `doc_unit` 是主读数的单位：行自己的单位与它相同时不写 `unit` 键
/// （同单位的行不必每行再标一遍「积分」）。
pub fn credit_rows(packs: &[Pack], is_cn: bool, doc_unit: &str) -> Vec<Value> {
    let mut rows: Vec<(i64, String, Value)> = Vec::new();
    for pack in active_packs(packs) {
        let quota = effective_quota(pack);
        let name = if pack.display_desc.is_empty() {
            product_type_identity(pack.product_type, is_cn).to_string()
        } else {
            pack.display_desc.clone()
        };
        let mut row = serde_json::Map::new();
        row.insert("name".to_string(), json!(name.clone()));

        // 额度口径逐包选（见函数头的说明）；`-1` 是「不限」，没有可写的数
        let mut unlimited = false;
        let (unit, total, used, remain) = if let Some(limit) = quota.credits_limit {
            if limit == -1 {
                unlimited = true;
                ("积分", None, None, None)
            } else {
                let used = pack.usage.credits_amount.unwrap_or(0.0);
                // 与官方 cashier 同款：先减再夹到非负（负数是超支，不写成负余额）
                ("积分", Some(limit as f64), Some(used), Some((limit as f64 - used).max(0.0)))
            }
        } else if let Some(limit) = quota.basic_usage_limit {
            // 套餐口径：剩余走 `pack_remain`（它已经处理了 bonus 的加法），
            // 不在这里另算一遍 —— 两处各算一次迟早会漂
            (
                "额度",
                Some(limit as f64),
                pack.usage.basic_amount.map(|value| value as f64),
                pack_remain(pack).map(|value| value as f64),
            )
        } else {
            ("", None, None, None)
        };
        for (key, value) in [("total", total), ("used", used), ("remain", remain)] {
            if let Some(value) = value {
                row.insert(key.to_string(), amount_json(value));
            }
        }
        if unlimited {
            row.insert("unlimited".to_string(), Value::Bool(true));
        }
        if !unit.is_empty() && unit != doc_unit {
            row.insert("unit".to_string(), json!(unit));
        }

        let expire = if pack.expire_time > 0 { pack.expire_time } else { pack.end_time };
        if pack.start_time > 0 {
            row.insert("grantAt".to_string(), json!(to_millis(pack.start_time)));
        }
        if expire > 0 {
            row.insert("expireAt".to_string(), json!(to_millis(expire)));
        }
        // 排序键：最近到期的排前面 —— 明细要回答的第一个问题是「哪笔先作废」。
        // 没给到期时刻的排最后（i64::MAX），同一时刻按名字稳定排序
        // （否则每次刷新行序都在跳，看起来像数据在变）
        rows.push((if expire > 0 { to_millis(expire) } else { i64::MAX }, name, Value::Object(row)));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    rows.into_iter().map(|(_, _, row)| row).collect()
}

/// 一次余额读取的全部结果（`document()` 的输入）。
/// 三个补充维度（`fast_request_per` / `solo_parallel` / `solo_package`）在这里
/// 已经是**定稿值**：先取 `pay_status`（与参考实现同口径），没取到才回退到
/// 权益包的 quota（文件末尾那条增补说明）。`document()` 因此只负责排版，
/// 不再决定"这条数据该从哪来"。
#[derive(Debug)]
pub struct Readout {
    pub is_credits_billing: bool,
    pub summary: Summary,
    pub pool: CreditsPool,
    pub plan: String,
    pub fast_request_per: Option<i64>,
    pub solo_parallel: Option<i64>,
    pub solo_package: bool,
    /// `pay_status` 的 `user_pay_identity_str`（免费档判定的第二个输入）。
    pub pay_plan_type: String,
    /// 官方 `usage_summary`（总额/已用/消耗比），只做交叉核对。
    pub official: Option<OfficialSummary>,
    /// 可见权益包的**原始清单**：`credits[]` 明细行的原料（见 [`credit_rows`]）。
    /// 主读数用的是它的聚合结果，明细用的是一份行一份包 —— 两处读同一份数据，
    /// 不各解析一遍上游响应。
    pub packs: Vec<Pack>,
    /// `ide_user_pay_status` 的原始响应；`None` = 那次查询失败或 `code != 0`。
    pub pay_status: Option<Value>,
    /// 上游原始响应（排障与对拍用，界面默认不展示）。
    pub raw_ent_usage: Value,
}

/// 把一次读取摊成**账号页的统一形状**（契约见 `ProviderAdapter::query_usage`）。
///
/// `available` 只在拿到**有限数**时给：-1（不限）不是数，写出去会被读成
/// "欠费 1 次"；它改由 `wallets[].balanceView` 用一句人话表达（"不限"）。
/// 一家源都没有有限数时给 `null` —— 界面因此显示「可用 —」，
/// 与"查到了 0"是两种读数，不能混。
///
/// `credits[]` 是**逐份权益包**的明细（领取 → 到期 + 面额/已用/剩余），
/// 与 `wallets`（这一列的聚合读数）两回事：钱在哪个包里、哪一份先作废，
/// 只有清单答得上来。有包才写这个键（见末尾的说明）。
///
/// `is_cn` 由调用方按账号谱系给定（本家只有 SOLO：CN=true）。
pub fn document(readout: &Readout, is_cn: bool) -> Value {
    let summary = &readout.summary;
    let pool = &readout.pool;

    let mut wallets: Vec<Value> = Vec::new();
    if pool.known {
        wallets.push(json!({
            "type": "credits_pool",
            "displayName": "积分池",
            "balance": if pool.unlimited { Value::Null } else { json!(pool.remain) },
            "unlimited": pool.unlimited,
            "balanceView": if pool.unlimited { UNLIMITED_VIEW.to_string() } else { format!("剩 {} 积分", pool.remain) },
        }));
    }
    // 没有任何额度证据（unknown 档）时**不给假行**：那格留给 raw + 「可用 —」，
    // 免得界面上出现一行"剩 0 / 共 0"看起来像读到了。
    if summary.model == MODEL_FAST {
        wallets.push(json!({
            "type": "fast_request",
            "displayName": "速通次数",
            "balance": if summary.remain < 0 { Value::Null } else { json!(summary.remain) },
            "unlimited": summary.remain < 0,
            "balanceView": fast_view(summary),
        }));
    } else if summary.model == MODEL_BASIC {
        wallets.push(json!({
            "type": "plan_quota",
            "displayName": "套餐余量",
            "balance": if summary.remain_known { json!(summary.remain) } else { Value::Null },
            "balanceView": if summary.remain_known {
                format!("剩 {} / 共 {}，已用 {}", summary.remain, summary.total, summary.used)
            } else {
                "—".to_string()
            },
        }));
    }
    if let Some(value) = readout.solo_parallel {
        wallets.push(json!({
            "type": "solo_parallel", "displayName": "SOLO 并发",
            "balance": value, "balanceView": format!("{value} 路"),
        }));
    }
    if readout.solo_package {
        wallets.push(json!({
            "type": "solo_package", "displayName": "SOLO 套餐",
            "balanceView": "可用",
        }));
    }
    if let Some(value) = readout.fast_request_per {
        wallets.push(json!({
            "type": "fast_request_per", "displayName": "快请求配额",
            "balance": value, "balanceView": format!("{value} 次/月"),
        }));
    }
    // 官方自己给的汇总（同一个池子、不逐包取整）：只在能算出来时给一行交叉核对
    if let Some(official) = &readout.official {
        let left = official.total - official.consumed;
        wallets.push(json!({
            "type": "usage_summary", "displayName": "官方汇总",
            "balance": (left * 100.0).round() / 100.0,
            "balanceView": format!(
                "剩 {} / 共 {}（已用 {}）",
                trim_amount(left),
                trim_amount(official.total),
                official.ratio.map(|value| format!("{:.1}%", value * 100.0)).unwrap_or_else(|| trim_amount(official.consumed).to_string())
            ),
        }));
    }

    // 主读数按"真正被扣的那笔钱"优先：积分制账户看积分池，其次速通次数，再次套餐余量。
    let (available, unit) = if pool.known && !pool.unlimited {
        (json!(pool.remain), "积分")
    } else if summary.model == MODEL_FAST && summary.remain >= 0 {
        (json!(summary.remain), "次")
    } else if summary.model == MODEL_BASIC && summary.remain_known {
        (json!(summary.remain), "额度")
    } else {
        (Value::Null, "积分")
    };

    let plan_type = readout.pay_plan_type.clone();
    // 悬停明细里的 `余量 N / 总量 N` 是**按数值**渲染的（界面写的是
    // `Number.isFinite(Number(v))`，而 JS 里 `Number(null) === 0`）——
    // 所以"没读到"必须**整个不写这个键**，写 null 会画成「余量 0」。
    let mut subscription = serde_json::Map::new();
    subscription.insert("planName".to_string(), json!(readout.plan));
    subscription.insert(
        "status".to_string(),
        json!(if is_free_plan(&readout.plan, &plan_type) { "免费" } else { "付费" }),
    );
    // 到期时间：上游给的是**秒**级 epoch（>1e11 才当作毫秒用），界面按毫秒判。
    if summary.end_time > 0 {
        subscription.insert("expireAt".to_string(), json!(to_millis(summary.end_time)));
    }
    let is_basic = summary.model == MODEL_BASIC && summary.remain_known;
    if is_basic {
        subscription.insert("remainQuota".to_string(), json!(summary.remain));
        if summary.total > 0 {
            subscription.insert("totalQuota".to_string(), json!(summary.total));
        }
    }

    // 逐份明细（行形状与 workbuddy 的 `credit_packs_from` 一致，界面只有一个
    // 读取器）：空数组**整个不写这个键** —— 界面按「有没有这个数组」决定要不要
    // 给这一格挂「可点开」的交互，给一个空数组会点出一个空框。
    let credits = credit_rows(&readout.packs, is_cn, unit);

    let mut raw = serde_json::Map::new();
    raw.insert("entUsage".to_string(), readout.raw_ent_usage.clone());
    raw.insert("usageModel".to_string(), json!(summary.model));
    raw.insert("remainKnown".to_string(), json!(summary.remain_known));
    raw.insert("creditsPoolKnown".to_string(), json!(pool.known));
    raw.insert("creditsPoolUnlimited".to_string(), json!(pool.unlimited));
    raw.insert("isCreditsBilling".to_string(), json!(readout.is_credits_billing));
    raw.insert("isCN".to_string(), json!(is_cn));
    if let Some(pay) = &readout.pay_status {
        raw.insert("payStatus".to_string(), pay.clone());
    }

    let mut document = serde_json::Map::new();
    document.insert("available".to_string(), available);
    document.insert("unit".to_string(), json!(unit));
    document.insert("wallets".to_string(), Value::Array(wallets));
    document.insert("subscription".to_string(), Value::Object(subscription));
    if !credits.is_empty() {
        document.insert("credits".to_string(), Value::Array(credits));
    }
    document.insert("raw".to_string(), Value::Object(raw));
    Value::Object(document)
}

/// 去掉浮点尾零（`1468.20` → `1468.2`、`1550.00` → `1550`）。
///
/// 上游这两个字段是浮点，直接 `{} →` 会打出 `1468.23` 与 `1550` 混着两种长度；
/// 积分本来就是"角分"级的小数，界面上不该看起来像度量值。
fn trim_amount(value: f64) -> String {
    if (value - value.round()).abs() < 0.005 {
        return format!("{}", value.round() as i64);
    }
    format!("{:.2}", value).trim_end_matches('0').trim_end_matches('.').to_string()
}

fn fast_view(summary: &Summary) -> String {
    if summary.remain < 0 {
        return format!("不限（已用 {}）", summary.fast_used);
    }
    if summary.fast_limit > 0 {
        return format!("剩 {} / 共 {} 次，已用 {}", summary.remain, summary.fast_limit, summary.fast_used);
    }
    format!("剩 {} 次", summary.remain)
}

/// 读一次额度（ent_usage 必成，pay_status 尽力而为）。
///
/// 401 原样透出（`status_for(SessionDead)`），由调用方走"刷新后重试一次"：
/// 这条族对**谱系**最敏感（参考实现 v0.12.60 记的就是跨类 token 时聊天照常用、
/// 只有积分查询报 1001），所以刷新必须在外面真接上，不能在报告里猜成"没有额度"。
pub async fn read(credential: &Credential, proxy: Option<&ResolvedProxy>) -> Result<Readout, GatewayError> {
    read_at(credential, proxy, UG_HOST).await
}

/// `read` 的内部实现，把 ug 域名做成**入参**。
///
/// 与 `forward::forward_at` 同一条道理：读环境变量会让同进程里并发的测试
/// 互相踩（谁先 set 谁的 host 就生效），而"打到真上游"的测试是**会花真实额度**的。
async fn read_at(credential: &Credential, proxy: Option<&ResolvedProxy>, base: &str) -> Result<Readout, GatewayError> {
    if !credential.valid() {
        return Err(GatewayError::with_status(401, "Trae 账号里没有可用凭证，无法查询额度"));
    }
    let prepared: Vec<(String, String)> =
        ug_headers(credential.variant(), credential.access_token.trim(), credential.device_id.trim()).into_iter().collect();
    let headers: Vec<(&str, String)> = prepared.iter().map(|(name, value)| (name.as_str(), value.clone())).collect();
    let body = json!({});
    // 两个地址一次算好（而不是两处各自 `format!`）：上一次就是"只把第一处换成
    // base、第二处仍是常量"，结果测试里那条 pay_status 请求打到了真上游。
    // 一处漏改不会报错，只会安静地花掉真实额度 —— 所以这里宁可只有一处拼接。
    let ent_usage_url = format!("{base}{EP_ENT_USAGE}");
    let pay_status_url = format!("{base}{EP_PAY_STATUS}");

    let ent_usage = post_ug(&ent_usage_url, &body, &headers, proxy).await?;
    let payload = ent_usage.json().unwrap_or_else(|| json!({}));
    let packs = parse_packs(&payload);
    let is_cn = true; // 本家只接 SOLO CN 通道（Intl 是另一套两步协议，未接）
    let is_credits_billing = is_credits_billing(&payload);
    let mut summary = summarize(&packs, is_cn);
    if summary.end_time == 0 {
        if let Some(selected) = select_active_pack(&packs, is_cn) {
            summary.end_time = selected.end_time;
        }
    }
    let pool = credits_pool_usage(&packs, is_credits_billing);
    let plan = plan_name(&packs, is_cn);

    // 第二条链路失败不阻塞：它的维度是**补充**，缺它只是少两行明细，
    // 而把整格报成失败会让人以为余额也没读到（与参考实现同一取舍）。
    let mut pay_status: Option<Value> = None;
    match post_ug(&pay_status_url, &body, &headers, proxy).await {
        Ok(reply) => {
            if let Some(payload) = reply.json() {
                if payload.is_object() {
                    pay_status = Some(payload);
                }
            }
        }
        Err(error) => {
            // 会话失效要往上抛：它同时会让 ent_usage 的结果变得可疑
            if error.status_code == 401 {
                return Err(error);
            }
            crate::logging::verbose("[Trae]", &format!("pay_status 补充维度没拿到（不影响积分读数）：{}", error.message));
        }
    }
    // 参考实现只在 `code == 0` 时采纳 pay_status 的维度；把这条判据留在这里，
    // 别让一个 `code:1001` 的失败体被当成一份合法配额。
    if pay_status.as_ref().is_some_and(|body| pay_code(body) != 0) {
        crate::logging::verbose("[Trae]", "pay_status 返回非 0 业务码，忽略其补充维度");
        pay_status = None;
    }

    // 三个补充维度在此定稿：pay_status 优先（与参考实现同口径），
    // SOLO 那两条没取到时回退到权益包的 quota（本机实测免费包就带着它们）。
    let fast_request_per = pay_status.as_ref().and_then(fast_request_per);
    let solo_parallel = pay_status.as_ref().and_then(solo_parallel_limit).or_else(|| solo_parallel_from_packs(&packs));
    let solo_package = pay_status.as_ref().is_some_and(has_solo_package) || solo_package_from_packs(&packs);
    let pay_plan_type = pay_status.as_ref().map(plan_identity).unwrap_or_default();

    Ok(Readout {
        is_credits_billing,
        summary,
        pool,
        plan,
        fast_request_per,
        solo_parallel,
        solo_package,
        pay_plan_type,
        official: official_summary(&payload),
        packs,
        pay_status,
        raw_ent_usage: payload,
    })
}

async fn post_ug(
    url: &str,
    body: &Value,
    headers: &[(&str, String)],
    proxy: Option<&ResolvedProxy>,
) -> Result<Reply, GatewayError> {
    let reply = post_json(url, body, headers, REQUEST_TIMEOUT, proxy).await?;
    if reply.status >= 400 {
        let head: String = reply.body.chars().take(ERROR_BODY_HEAD).collect();
        let kind = classify(reply.status, &head);
        let message = format!("Trae 积分接口返回 {}: {}", reply.status, head.trim());
        return Err(GatewayError::with_status(i32::from(status_for(kind)), message));
    }
    Ok(reply)
}

/// 错误类别 → 客户端可见状态码（与 `forward.rs` **同一张表**，见那里的注释）。
fn status_for(kind: super::errors::ErrorKind) -> u16 {
    super::forward::status_for(kind)
}

/// 查询某账号的额度（`ProviderAdapter::query_usage` 的实现）。
pub async fn query_usage(ctx: &PluginCtx, account_id: &str) -> Result<Value, GatewayError> {
    let record = read_record(ctx, account_id).await?;
    let credential = Credential::from_payload(&record).map_err(GatewayError::new)?;
    let proxy = account_proxy(&record)?;
    // 临期先续期：这条族对谱系最敏感，拿一个刚被服务端作废的串去问余额，
    // 得到的是 401「unable to authenticate」而不是"额度为 0"，报错了也白报。
    let credential = renew_if_due(ctx, &record, &credential, proxy.as_ref()).await?;
    let readout = read(&credential, proxy.as_ref()).await?;
    Ok(document(&readout, true))
}

