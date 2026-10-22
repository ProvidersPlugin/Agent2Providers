//! 随机标识：一次性 state、请求 id、幂等键。
//!
//! ── 为什么自己生成而不是问网关要 ────────────────────────────
//! 登录 state 与签到幂等键都是**插件自己那一轮**的凭据（网关不需要知道、
//! 也不该存一份）。走 RPC 拿一个随机串既增加一次往返，又让「这个 state 是
//! 谁生成的」变成两方共享的事实 —— 而它本来就只属于插件。
//!
//! ── 随机源 ──────────────────────────────────────────────────
//! `getrandom`（与网关侧同一份依赖）。取不到时退回「纳秒时间戳 + 进程内
//! 单调计数」派生的形态：不是密码学安全，但仍然是不可预测度合理的唯一串，
//! 比拿一个常量或空串去当 state 安全得多。

use std::sync::atomic::{AtomicU64, Ordering};

/// 取 16 个随机字节（随机源失败时用时间戳 + 计数器兜底）。
fn random_bytes() -> [u8; 16] {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_ok() {
        return bytes;
    }
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    bytes[..8].copy_from_slice(&nanos.to_be_bytes());
    bytes[8..].copy_from_slice(&counter.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_be_bytes());
    bytes
}

/// 生成一个 UUID v4 形态的串（`8-4-4-4-12`，带横线）。
///
/// 版本位与变体位按 RFC 4122 写：有些上游（OrcaTerm 的 `sessionId`）会校验
/// 形态，非 UUID 直接回 `api-params-verify-fail`。
pub fn uuid_v4() -> String {
    let mut bytes = random_bytes();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let part = |start: usize, end: usize| hex.get(start..end).unwrap_or("").to_string();
    format!(
        "{}-{}-{}-{}-{}",
        part(0, 8),
        part(8, 12),
        part(12, 16),
        part(16, 20),
        part(20, 32)
    )
}

/// `<prefix>_<32 位 hex>` 形态的随机标识（网关 `protocol::random_id` 的同构物）。
///
/// Anthropic 协议的 `toolu_…` / `msg_…` 用它：那些 id 的**前缀有意义**
/// （上游按前缀判类型），所以不能换成 [`uuid_v4`] 的横线形态。
pub fn random_id(prefix: &str) -> String {
    let bytes = random_bytes();
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{prefix}_{hex}")
}

// ─── WorkBuddy 的会话头族（UUIDv7 家族）─────────────────────────
//
// 与上面两个（v4 / `prefix_hex`）不是一回事：WorkBuddy 的上游按**形态**认
// 这几个头 —— 会话 id 是带连字符的 UUID，请求 / 消息 / span id 是 32 位或
// 16 位无连字符 hex，且都要求 v7（时间前缀）。用 v4 的随机字节也能过，但
// 那是自造差异；这里与网关 `upstream::request::uuid7_bytes` 逐字相同。

/// 16 字节 UUIDv7（毫秒时间前缀 + 版本位 + 随机尾）。
fn uuid7_bytes() -> [u8; 16] {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mix = |label: &[u8], salt: u64| -> u64 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write(label);
        hasher.write_u64(salt);
        hasher.finish()
    };
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
        & ((1 << 48) - 1);
    let rand_a = mix(b"wb-uuid7-a", counter) & 0x0fff;
    let rand_b = mix(b"wb-uuid7-b", counter.wrapping_mul(0x9E37_79B9_7F4A_7C15)) & ((1 << 62) - 1);
    let int = ((millis as u128) << 80)
        | (0x7u128 << 76)
        | ((rand_a as u128) << 64)
        | (0b10u128 << 62)
        | (rand_b as u128);
    int.to_be_bytes()
}

/// 请求 / 消息 id（`X-Request-ID` 那四个头）：32 位无连字符 hex。
pub fn new_message_id() -> String {
    uuid7_bytes().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 会话 id（`X-Conversation-ID`）：带连字符的 UUID 形态。
pub fn new_conversation_id() -> String {
    let bytes = uuid7_bytes();
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

/// 链路追踪的 span id（`traceparent` / b3 / `X-B3-SpanId`）：16 位 hex。
pub fn new_span_id() -> String {
    uuid7_bytes()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
