//! ModelScope 的**账号级失败判据**：两类不该被当成「速率窗口」的拒绝。
//!
//! ── 为什么要单独一个文件 ────────────────────────────────────
//! 魔搭把「账号条件不满足」和「速率窗口撞上」都用 429 / 403 表达，**只能靠文案
//! 区分** —— 而两者的正确处置完全不同：前者在充值 / 完成验证之前**一直**不可用
//! （按天数冷却），后者等一会儿就好（分钟级冷却）。
//!
//! ── 两类是什么（附上游真实报文）──────────────────────────────
//! ```text
//!   ① 魔粒耗尽   429 {"error":{"message":"insufficient balance","request_id":"…"}}
//!       账号的魔粒用完了。充魔粒之前这把令牌确实不可用 —— 按「当日禁用」处理，
//!       跨过午夜再试一次（充了值就恢复）。
//!   ② 未验证邮箱 403 正文含 "email verification" / "verified accounts" /
//!       "/my/settings/account"
//!       账号没完成邮箱验证，魔粒推理接口直接拒绝服务，充魔粒也救不回来 ——
//!       唯一出路是去账号设置页完成验证。
//! ```
//!
//! ── 插件里还有一条，本实现**有意不照搬**（说明理由，免得将来被当成遗漏）──
//! 参考插件把 `503 + "model_not_ready"`（上游 vLLM 还在加载权重）当作**整家
//! 不可用**摘掉。本实现**不做**：那件事与账号无关（本家每把令牌都会被同样
//! 拒绝），罚账号是错的。这里的默认处置是 `Fatal`（不罚账号，如实回给客户端、
//! 由客户端重试）。
//!
//! ⚠️ **判据一律大小写不敏感的子串匹配，且只在「这次请求已经失败」的分支上
//! 调用** —— 子串匹配本身不区分「上游报的错」与「模型自己生成的内容」。

/// 一整天（毫秒）
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// 北京时间的固定偏移（UTC+8）。
///
/// 用**固定偏移**而不是机器时区：网关可能跑在 UTC 容器里，而魔搭的一整套
/// 「当日」口径（签到、魔粒发放、日额度）都是按北京时间算的 ——「明天零点」
/// 必须是北京的那个。
const DAY_ZONE_OFFSET_SECONDS: i32 = 8 * 3600;

/// 一次失败是否意味着**账号魔粒耗尽**（429 + `insufficient balance`）。
///
/// 只看文案不看状态码也可以，但两样都看更稳 —— 别的 4xx 里出现这串字的
/// 场合不存在，多一道闸门不损失什么。
pub fn is_insufficient_balance(status: u16, message: &str) -> bool {
    status == 429 && contains_ci(message, "insufficient balance")
}

/// 一次失败是否意味着**账号未完成邮箱验证**（403 + 三选一的特征串）。
///
/// **必须同时看状态码与文案**：403 还有别的权限语义，只看状态码会把无关失败
/// 误判成「账号未验证」，而那会让用户去查一个不存在的问题。
pub fn is_unverified_account(status: u16, message: &str) -> bool {
    if status != 403 {
        return false;
    }
    ["email verification", "verified accounts", "/my/settings/account"]
        .iter()
        .any(|marker| contains_ci(message, marker))
}

/// 大小写不敏感的子串匹配（空值一律 false）
fn contains_ci(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

/// `now_ms` 之后最近的**北京时间零点**（毫秒时间戳，纯函数）。
///
/// 给「魔粒耗尽 / 账号未验证 ⇒ 冷却到明天」用：这两种状态在充值 / 完成验证
/// 之前一直不可用，而魔搭的日口径是北京零点 —— 跨过那个点再试一次，既不会
/// 在当天反复白撞，也不会把账号永久钉死。
///
/// 边界：恰好落在零点上时给**次日**零点。用 `div_euclid` 而不是 `/`：
/// 负数时刻（1970 前）在 Rust 里默认向零取整会算错整天边界。
pub fn next_day_boundary_ms(now_ms: i64) -> i64 {
    let shifted = now_ms + i64::from(DAY_ZONE_OFFSET_SECONDS) * 1000;
    let start_of_today = shifted.div_euclid(DAY_MS) * DAY_MS;
    start_of_today + DAY_MS - i64::from(DAY_ZONE_OFFSET_SECONDS) * 1000
}
