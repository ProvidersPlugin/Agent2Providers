//! CatPaw 入口的**请求体字节上限**（issue #6）。
//!
//! ── 这道墙是什么 ────────────────────────────────────────────
//! `ai.catpaw.meituan.com` 前面是 openresty，它按**请求体字节数**拒收，不按
//! token 窗口。实测（issue #6 的表）：
//!
//! ```text
//!   345K 个中文字符 ≈ 1.03 MB  → 200
//!   350K 个中文字符 ≈ 1.05 MB  → 413 Request Entity Too Large（openresty 默认 HTML）
//! ```
//!
//! 中文字符在 UTF-8 里 3 字节，所以「345K 字」与「1 MiB」是同一堵墙的两种量法
//! —— 不是模型窗口只有 345K token。上游对**声明**的窗口（1M）没有意见，被拒的
//! 只是这一次发出去的字节。
//!
//! ── 为什么在本家做（而不是让通用层管）───────────────────────
//! 墙在 CatPaw 的入口上，各家入口的形态完全不同（CodeArts 是 6 MiB + 12 MiB
//! 两道，Loomy 是 APIG 的 12 MB）。通用层没有可共享的判据，硬塞进去只会得到
//! 「所有家一起按最小值收敛」。
//!
//! ── 三条口径 ────────────────────────────────────────────────
//!   · **发送前就拦**：超限的体发出去只会换回一页 openresty HTML，白等一个
//!     往返；本地直接给 413 + 人话，客户端当场就能改。
//!   · **广告的窗口要够得着**：`maxInputTokens` 声明 1M、而 1 MiB 体只装得下
//!     二十几万 token，下游按声明把窗口配到 400K 就必然撞墙（issue #6 的
//!     现象）。所以清单里按「实际发得出去」收敛，并把字节上限单独声明出来。
//!   · **上游真回了 413 也要翻成人话**：本地估算可能比上游松（图片 base64、
//!     注入的系统提示词都会占字节），漏网时不能把 openresty 的 HTML 原样
//!     丢给客户端。

use crate::errors::GatewayError;

use super::models::CatPawError;

/// 入口的请求体上限：**1 MiB**（1,048,576 字节）。
///
/// 取整到 MiB 而不是实测那个 1.03 MB：intermediary 的限流一般就配在整 MiB 上，
/// issue #6 里 345K 字（1.03 MB）能过、350K 字（1.05 MB）被拒，两条读数夹住的
/// 正是这个值。
pub const ENTRY_BODY_LIMIT_BYTES: i64 = 1_048_576;

/// 体积拒绝过线用的状态码。
///
/// 单独摘出来是因为它不只用于文案：宿主靠这个码认出「同家换号无效」
/// （见 [`conversation_wire_message`]）。
pub const ENTRY_BODY_LIMIT_STATUS: u16 = 413;

/// 广告给下游的上下文窗口：**200K**。
///
/// ── 为什么是 200K 而不是 1M 或 262144 ────────────────────────
///   · 1 MiB 体按最坏情况（4 字节 / token，代码与英文）只装得下约 262K token，
///     再算上注入的 system 提示词与消息信封，实际更少 ⇒ 1M 是够不着的；
///   · 262144 虽然更贴近上限，但客户端的窗口档位是 200K / 500K / 1M 这类整数
///     档，报一个中间值会被就近取整到 500K，等于又回到会撞墙的那一档；
///   · 200K 是各家客户端都有的最低档，200K token 的中文 ≈ 600 KB、代码 ≈
///     800 KB，都在 1 MiB 之内，且留得下系统提示词的余量。
///
/// 用户仍可在模型管理页把窗口覆盖成更大的值（覆盖层优先级最高）—— 那是有意识
/// 的选择，而默认值不该把用户直接送进 413。
pub const ADVERTISED_CONTEXT_WINDOW: i64 = 204_800;

/// 一条清单条目的窗口该取多少：上游声明的档位上限与「发得出去」的上限取小。
///
/// `declared` 是上游（远程目录或静态表）给的档位上限；`None` 表示这家没声明，
/// 这时用 [`ADVERTISED_CONTEXT_WINDOW`]。
pub fn advertised_context_window(declared: Option<i64>) -> i64 {
    match declared {
        Some(window) if window > 0 => window.min(ADVERTISED_CONTEXT_WINDOW),
        _ => ADVERTISED_CONTEXT_WINDOW,
    }
}

/// 这一发是不是被入口**按体积**拒了。
///
/// 判据只取状态码 413 与两句文案：
///   · openresty 默认页带 `413 Request Entity Too Large`；
///   · 有些网关只在 body 里写 `Request entity too large`（大小写不敏感）。
/// 不按「HTML 里有没有 413」猜：403 / 502 的默认页也带数字，误判会把一次凭据
/// 失效说成体积问题。
pub fn is_entry_rejection(status: u16, text: &str) -> bool {
    if status == 413 {
        return true;
    }
    let lowered = text.to_ascii_lowercase();
    lowered.contains("request entity too large") || lowered.contains("payload too large")
}

/// 本地估算超限时的错误（**没有发出任何字节**）。
///
/// 状态码给 413 而不是 502：这是「这份体太大」，客户端据此提示用户减小上下文
/// 才是正确动作；502 会被读成上游故障、诱发重试同一份体。
pub fn oversized_error(bytes: i64) -> CatPawError {
    CatPawError::http(
        ENTRY_BODY_LIMIT_STATUS,
        format!(
            "CatPaw 入口按请求体字节数限流：本次约 {bytes} 字节，超过约 1 MiB\
             （{} 字节）上限，已在本地拦下、没有发往上游。请减小上下文\
             （少带历史 / 拆分粘贴的文件），或把客户端上下文窗口调到 200K 档",
            ENTRY_BODY_LIMIT_BYTES
        ),
    )
}

/// 上游真回了体积拒绝时的错误（把 openresty 的 HTML 换成同一句人话）。
///
/// 末尾那句「换账号无效」是有用的：会话式路径以前会把这个错误当普通 Fatal
/// 顺延到下一个账号，每个账号都白打一遍同样的体积。
///
/// 出口状态码**统一成 413**，即便入口回的是 400：判定依据是「这份字节太大」，
/// 而 413 是客户端唯一会据以「缩小上下文」的码（400 会被读成请求格式错误，
/// 用户改了历史也未必好）。上游原码写进文案，排查时仍对得上日志。
pub fn upstream_rejection_error(status: u16) -> CatPawError {
    CatPawError::http(
        ENTRY_BODY_LIMIT_STATUS,
        format!(
            "CatPaw 上游入口拒收这份请求体（上游回的是 HTTP {status}，上限约 1 MiB / \
             {} 字节）：请减小上下文（少带历史 / 拆分文件）或把窗口调到 200K 档。\
             这是体积问题、不是账号问题，换账号重试无效",
            ENTRY_BODY_LIMIT_BYTES
        ),
    )
}

/// 发送前的统一闸门：超限返回错误，未超限放行。
pub fn check_body_limit(bytes: i64) -> Result<(), CatPawError> {
    if bytes > ENTRY_BODY_LIMIT_BYTES {
        return Err(oversized_error(bytes));
    }
    Ok(())
}

/// 会话路径的错误怎么过线（只在这家改写，别家仍走 `into_message`）。
///
/// ── 为什么不能一律 `into_message` ──────────────────────────────
/// `into_message` 只留文案，状态码在过线时丢掉，宿主看到的是 502。宿主的
/// 「体积拒绝不换号」判据读的正是状态码（`PAYLOAD_BOUND_STATUSES`）——
/// 只留文案时它**永远命中不了**，issue #6 的诉求 3 就只剩一句人话文案，
/// 队列里每个账号照旧白撞一遍。
///
/// ── 为什么也不一律 `into_control_message` ──────────────────────
/// 那会把 401 / 429 这些码一并带给宿主，顺带改变本家的既有语义（本家的分类
/// 恒为 Fatal，这是原实现的核对结论，见 `adapter.rs` 模块头）。issue #6 只需要
/// 体积这一档，其余仍按文案透传。
pub fn conversation_wire_message(error: GatewayError) -> String {
    if error.status_code == i32::from(ENTRY_BODY_LIMIT_STATUS) {
        error.into_control_message()
    } else {
        error.into_message()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_over_one_mib_is_refused_before_it_is_sent() {
        assert!(check_body_limit(ENTRY_BODY_LIMIT_BYTES).is_ok(), "正好一 MiB 不拦");
        let error = check_body_limit(ENTRY_BODY_LIMIT_BYTES + 1)
            .expect_err("一 MiB 以上必须拦下");
        assert_eq!(413, error.status, "客户端要看到 413 而不是 502");
        assert!(error.message.contains("1 M"), "文案要说清上限量级：{}", error.message);
        assert!(
            error.message.contains("没有发往上游"),
            "必须说明这一发没有出门，排查时才不会去上游找：{}",
            error.message
        );
    }

    #[test]
    fn the_advertised_window_is_never_above_what_one_mib_can_carry() {
        assert_eq!(
            ADVERTISED_CONTEXT_WINDOW,
            advertised_context_window(Some(1_024_000)),
            "上游声明 1M 也要收敛到发得出去的档位"
        );
        assert_eq!(
            131_072,
            advertised_context_window(Some(131_072)),
            "上游声明的窗口更小时不该被抬高"
        );
        assert_eq!(
            ADVERTISED_CONTEXT_WINDOW,
            advertised_context_window(None),
            "没声明时用同一个保守档位"
        );
    }

    #[test]
    fn a_real_413_is_recognised_and_rewritten_into_plain_language() {
        // openresty 的默认页（HTML，正文里带这句）
        let html = "<html><head><title>413 Request Entity Too Large</title></head></html>";
        assert!(is_entry_rejection(413, html));
        assert!(is_entry_rejection(413, ""));
        assert!(is_entry_rejection(400, "Request entity too large"));
        // 反面对照：别的 HTML 错误页不能被误判成体积问题
        assert!(!is_entry_rejection(403, "<html><title>403 Forbidden</title></html>"));
        assert!(!is_entry_rejection(502, "<html><title>502 Bad Gateway</title></html>"));
        let error = upstream_rejection_error(413);
        assert_eq!(413, error.status);
        assert!(
            error.message.contains("换账号重试无效"),
            "要明确否掉换账号这条无效路径：{}",
            error.message
        );
        // 入口回 400 但文案是体积拒绝时，出口仍是 413（客户端的处置动作不同）
        let downgraded = upstream_rejection_error(400);
        assert_eq!(413, downgraded.status);
        assert!(
            downgraded.message.contains("HTTP 400"),
            "上游原码要留在文案里：{}",
            downgraded.message
        );
    }

    /// 413 必须**带状态码**过线：宿主靠它认出「同家换号无效」。
    ///
    /// 改回 `into_message` 时这条会红 —— 那正是 issue #6 修不到位的样子：
    /// 文案很好、宿主却在按 502 处理，队列里的账号照旧白撞。
    #[test]
    fn a_body_limit_error_carries_its_status_over_the_wire() {
        let wire = conversation_wire_message(oversized_error(2_000_000).to_gateway());
        assert!(
            wire.contains("\"status\":413"),
            "413 要真的进信封，宿主才可能停在同家不再换号：{wire}"
        );
        assert!(
            wire.contains("已在本地拦下"),
            "文案不能因为换了个信封就丢：{wire}"
        );
        // 反面对照：别的错误仍只过文案，不把 401 / 429 这类码顺带改语义。
        let other = conversation_wire_message(CatPawError::upstream("上游请求失败").to_gateway());
        assert!(!other.contains("CONTROL_ERROR:"), "非体积错误不该改过线形态：{other}");
        assert_eq!("上游请求失败", other);
    }
}
