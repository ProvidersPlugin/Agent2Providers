//! 一次计费接口调用的最小实现（网关 `BillingService::call_billing` 的插件侧等价物）。
//!
//! ── 与网关那份的差别 ────────────────────────────────────────
//! 网关那版带一套 `CallOptions`（签到重复领取容错、白名单头、查询串、跨站落点
//! 等）。这里把它们收进 [`Options`]：签到 / 活动 / 一次性奖励搬进来之后，
//! 那几项**都**有调用方了，但默认值仍是「只查积分」那一套 —— 查积分的调用点
//! 因此不必写一堆 `..Default::default()`。
//!
//! 判定顺序：401 身份失效、其它非 2xx、业务码失败；403 保留上游拒绝原因。
//!
//! ── 头集合为什么不复用转发那套 ──────────────────────────────
//! 计费版的 `build_headers` 与转发版的 `build_auth_headers` **不能互相替换**：
//! 前者固定带 `Content-Type` / `Accept`、`X-User-Id` 缺省给空串、无 token 也给
//! `Bearer `，且不带 `X-Department-Info`。这些差别是上游两个 buildHeaders 各自
//! 的实现决定的（见 `billing::headers`）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::{json, Value};

use agent2api_provider_protocol::WireProxy as ResolvedProxy;

use crate::errors::GatewayError;
use crate::http::send_raw;
use crate::logging;

use crate::workbuddy_shared::endpoints::{
    normalize_endpoint, DEFAULT_ENDPOINT, RESPONSE_CODE_OK,
};
use super::headers::{build_headers, whitelist_headers};

/// 计费接口的请求超时（与网关 `billing::REQUEST_TIMEOUT_MS` 同档）。
const REQUEST_TIMEOUT_MS: u64 = 20_000;

/// 一次计费调用的成果（与网关 `BillingCall` 逐字段对应）。
pub(super) struct BillingCall {
    /// 上游业务码（`None` = 响应里没有这个字段）。
    pub(super) code: Option<i64>,
    /// 上游文案（`msg` 优先，回退 `message`；空串当没有）。
    pub(super) msg: Option<String>,
    /// 上游 `requestId`（签到失败分支会原样回给前端）。
    pub(super) request_id: Option<Value>,
    /// `data` 段（缺失为 `Null`）
    pub(super) data: Value,
    /// 整份响应体（企业额度的取值链要它）
    pub(super) raw: Option<Value>,
}

/// 一次调用的选项（网关 `CallOptions` 的同义物）。
///
/// 默认值 = 「只查积分」那条链的形态：会话端点、不带查询串、不带白名单头、
/// 非 0 业务码即失败、不容错重复领取。
pub(super) struct Options<'a> {
    /// 查询串（含前导 `?` 或为空）
    pub(super) query: Option<&'a str>,
    /// 落点覆盖：`None` = 会话端点；`Some` = 固定站点。
    ///
    /// 只有**跨站**的一次性运营奖励用它（新手礼包 / 活动补偿挂在国内版计费站
    /// `www.codebuddy.cn` 上）：落点由上游运营侧固定，不跟账号的 `endpoint` 走
    /// （账号端点可能是 `copilot.tencent.com` 这类旧出口，而这两条是后加的）。
    pub(super) base: Option<&'a str>,
    /// 是否带运营活动需要的客户端白名单头（只有 banner 需要）
    pub(super) whitelist_headers: bool,
    /// 非 0 业务码是否算失败
    pub(super) expect_code_ok: bool,
    /// 把「今天已签到 / 已领取」的 400/409 放行成 `Ok`（见 [`is_duplicate_claim`]）
    pub(super) tolerate_duplicate_claim: bool,
}

impl Default for Options<'_> {
    fn default() -> Self {
        Self {
            query: None,
            base: None,
            whitelist_headers: false,
            expect_code_ok: true,
            tolerate_duplicate_claim: false,
        }
    }
}

/// 调一次计费接口。
///
/// `path` 是 `/v2/...` 形态；落点取**会话端点**（账号自己的站点），
/// 除非 [`Options::base`] 给了固定站点。
pub(super) async fn call(
    method: &str,
    path: &str,
    body: Option<&Value>,
    session: &Value,
    proxy: Option<&ResolvedProxy>,
    locale: Option<&str>,
    options: Options<'_>,
) -> Result<BillingCall, GatewayError> {
    // 账户级端点：国内版/国际版账号各走自己的站点。会话缺 endpoint 时回落到
    // 该账号所属地区的默认端点（由调用方在 session 里补好，见 `workbuddy::mod`）。
    let endpoint = session
        .get("endpoint")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(normalize_endpoint)
        .unwrap_or_else(|| normalize_endpoint(DEFAULT_ENDPOINT));
    let base = options.base.map(str::to_string).unwrap_or(endpoint);
    let url = format!("{base}{path}{}", options.query.unwrap_or(""));

    let mut extra: Vec<(String, String)> = Vec::new();
    if let Some(locale) = locale {
        extra.push(("Accept-Language".to_string(), accept_language(Some(locale))));
    }
    if options.whitelist_headers {
        extra.extend(whitelist_headers(session));
    }
    let headers = build_headers(session, &extra);
    let payload_body = if method == "GET" || method == "HEAD" {
        None
    } else {
        // Node 是 `JSON.stringify(body ?? spec.body ?? {})` —— 缺省体一律是空对象，
        // 不能省掉（上游对缺 body 的 POST 会报 400）
        Some(body.cloned().unwrap_or_else(|| json!({})))
    };

    logging::verbose(
        "[Billing]",
        &format!(
            "{method} {url}{}",
            match proxy {
                Some(proxy) if !proxy.host.is_empty() => format!(" 经代理 {}", proxy.host),
                _ => String::new(),
            }
        ),
    );

    let response = send_raw(
        method,
        &url,
        payload_body.as_ref(),
        &headers,
        proxy,
        Some(REQUEST_TIMEOUT_MS),
    )
    .await
    .map_err(|error| {
        if error.is_timeout() {
            GatewayError::with_status(504, "计费接口请求超时")
        } else {
            GatewayError::with_status(502, format!("计费接口请求失败: {error}"))
        }
    })?;

    let payload = response.payload;
    let code = payload
        .as_ref()
        .and_then(|value| value.get("code"))
        .and_then(Value::as_i64);
    // Node: `payload?.msg || payload?.message` —— 空串也当「没有」，所以过滤空串
    let msg = payload
        .as_ref()
        .and_then(|value| {
            value
                .get("msg")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .or_else(|| {
                    value
                        .get("message")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                })
        })
        .map(str::to_string);
    // Node 的 `requestId: payload?.requestId` 在缺失时是 undefined，
    // JSON 化会**丢掉这个键**；用 Option 表达同一语义（None = 不出键）
    let request_id = payload
        .as_ref()
        .and_then(|value| value.get("requestId"))
        .cloned();

    if response.status == 401 {
        // ── 为什么文案里带完整 URL（issue #381）────────────────────
        // 401 有两种成因，处置完全相反：
        //   ① 凭证真的失效 → 重新登录；
        //   ② 账号的 `endpoint` 拼错 / 落到了另一个地区的站点 → 改端点，
        //      重新登录也没用。
        // 原来两种情况共用「登录态已过期或被拒绝」，把用户往 ① 带。而排查时
        // 最缺的信息正是「这次到底打了哪个地址」—— 只给路径片段（日志里那条
        // `auth/token/refresh`）看不出域名错没错。所以这里把请求的完整 URL
        // 写进错误：一眼能看出打的是国内版还是国际版站点。
        return Err(GatewayError::with_status(
            401,
            format!(
                "登录态被上游拒绝（HTTP 401，请求地址 {url}）：若该地址与账号所在地区不符，\
                 请检查账号的端点（endpoint）配置；地址正确时才是凭证失效，需要重新登录"
            ),
        )
        .with_optional_code(code));
    }
    if !response.ok {
        let detail = payload
            .as_ref()
            .and_then(|value| {
                value
                    .get("msg")
                    .and_then(Value::as_str)
                    .or_else(|| value.get("message").and_then(Value::as_str))
            })
            .unwrap_or("");
        // ── 「今天已签到」不是错误（issue #137）────────────────────
        // 上游对重复领取回的是 **HTTP 400 +「今天已签到，请明天再来」**，而不是
        // 200 + 非 0 code。不放行的话这条正常终态会被丢成 `Err`，于是 claim 为
        // null —— 面板的「已签到」永远不亮、定时签到的 lastResult 恒显示
        // 「成功领取 0/N」，用户会反复重试。
        //
        // 判据刻意收得很紧，只放行**明确说「今天已领过」**的那一种：
        //   · 只认 400 / 409（400 实测，409 防御性放行）；
        //   · 只认文案含「已签到 / 已领取」—— 与网关 `checkin_completed_today`
        //     的第 3 条判据同源，两处必须一致；
        //   · 只在签到领取那条调用点上开（`tolerate_duplicate_claim`）。
        if options.tolerate_duplicate_claim {
            if is_duplicate_claim(response.status, detail) {
                logging::verbose(
                    "[Billing]",
                    &format!(
                        "签到重复领取（HTTP {}），按「今日已完成」处理: {detail}",
                        response.status
                    ),
                );
                return Ok(BillingCall {
                    code,
                    msg,
                    request_id,
                    data: Value::Null,
                    raw: payload,
                });
            }
            // 开了容错却没命中：最可能是上游改了文案（判据是文案匹配）。
            // 留一条现场 —— 否则「面板又不像 #137 那样亮了」会查无实据。
            logging::verbose(
                "[Billing]",
                &format!(
                    "签到领取返回 HTTP {}，未命中「重复领取」文案，按错误处理: {detail}",
                    response.status
                ),
            );
        }
        let message = if detail.is_empty() {
            format!("计费接口返回 HTTP {}", response.status)
        } else {
            format!("计费接口返回 HTTP {}: {detail}", response.status)
        };
        return Err(
            GatewayError::with_status(i32::from(response.status), message)
                .with_optional_code(code),
        );
    }
    if let Some(code_value) = code {
        if code_value != RESPONSE_CODE_OK && options.expect_code_ok {
            let message = msg
                .clone()
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| format!("计费接口返回 code={code_value}"));
            return Err(
                GatewayError::with_status(i32::from(response.status), message)
                    .upstream_code(code_value),
            );
        }
    }

    let data = payload
        .as_ref()
        .and_then(|value| value.get("data"))
        .cloned()
        .unwrap_or(Value::Null);
    Ok(BillingCall {
        code,
        msg,
        request_id,
        data,
        raw: payload,
    })
}

/// 这次非 2xx 响应是否表示「今天已经领过了」（签到重复领取）。
///
/// 判据刻意收得很紧 —— 不能宽到「只要没报错就算已签到」，否则网络错误、
/// 凭证失效会被误判成已签到而白丢一天：
///   · **只认 400 / 409**：400 是实测的（当日已签到就是 400 +「今天已签到，
///     请明天再来」）；409 是防御性放行（某些实现用 409 Conflict 表达重复）。
///     500/502/504 是真故障，放行会让一次没签上的日子被记成已签，**绝不放**；
///   · **只认文案命中**：与网关 `checkin_completed_today` 第 3 条判据同源。
///     两处必须一致 —— 只放宽状态码而文案判不出来，等于放行了却仍显示「未签到」；
///   · 401/403 在调用点之前就被拦下（登录态问题），到不了这里。
///
/// `detail` 是 `msg`（或回退的 `message`）原文，空串表示上游没给文案 ——
/// 空串不含关键词，自然落到「不放过」，正是想要的失败方向。
pub(super) fn is_duplicate_claim(status: u16, detail: &str) -> bool {
    matches!(status, 400 | 409) && (detail.contains("已签到") || detail.contains("已领取"))
}

/// `Accept-Language` 的取值（网关 `BillingService::accept_language` 的同义物）。
///
/// 上游按它挑文案语种；本家只区分「中文」与「其余」—— 与网关那份一致。
fn accept_language(locale: Option<&str>) -> String {
    match locale.map(str::trim).filter(|value| !value.is_empty()) {
        Some(locale) if locale.to_ascii_lowercase().starts_with("zh") => "zh-CN".to_string(),
        Some(locale) => locale.to_string(),
        None => "zh-CN".to_string(),
    }
}

#[cfg(test)]
mod tests {
    //! `is_duplicate_claim` 的边界：只有「重复领取」那一种非 2xx 才该被放行。
    //! 其余一律保持报错 —— 把真故障静默成「已签到」会让用户白丢一天的积分。
    use super::is_duplicate_claim;

    /// 上游真实的 400 文案（当日已签到时的原话）。
    const REAL_DUPLICATE: &str = "今天已签到，请明天再来";

    #[test]
    fn real_duplicate_message_is_recognized() {
        assert!(is_duplicate_claim(400, REAL_DUPLICATE));
        assert!(is_duplicate_claim(409, REAL_DUPLICATE));
    }

    #[test]
    fn both_claim_wording_variants_are_recognized() {
        assert!(is_duplicate_claim(400, "今天已领取过了"));
        assert!(is_duplicate_claim(409, "该奖励已签到"));
    }

    #[test]
    fn only_400_and_409_are_tolerated() {
        for status in [500, 502, 503, 504, 429, 404] {
            assert!(
                !is_duplicate_claim(status, REAL_DUPLICATE),
                "status={status} 不该被放行"
            );
        }
    }

    #[test]
    fn non_duplicate_message_still_fails() {
        for detail in ["参数错误", "请求内容不是有效 JSON", "活动已结束", ""] {
            assert!(!is_duplicate_claim(400, detail), "detail={detail:?} 不该被放行");
        }
    }
}
