//! KukuAI 登录：**主站网页登录** + 壳侧读完整会话 Cookie 收尾。
//!
//! ── 为什么不是纯 API 短信、也不是 passport 页面自动化 ─────────
//! 百度 passport 的短信发码（`senddpass`）带一串 wappass 风控签名参数（由
//! 页面 JS 的风控库生成），纯 HTTP 复刻既不稳定也随时可能随版本失效
//! （2026-10-10 实测 `getcodetype` 等旧接口已 404）。passport 页面自动化
//! 也试过并放弃：登录页 DOM 与风控行为随版本变，自动化链路脆弱。
//! 最终方案把「怎么登录」还给用户：打开 KukuAI **主站**，用户用手机验证码 /
//! 扫码正常登录（官方页面自己处理风控），壳侧从 WebView2 的 Cookie 存储读
//! 完整会话交回。
//!
//! ── 流程（回调型：`poll_interval_ms = 0`）────────────────────
//!   1. 插件 `login.start` 返回主站地址 `https://kuku.baidu.com/`；
//!   2. 壳侧打开内嵌窗口（可见），用户在其中完成登录；
//!   3. 壳侧每 4 秒读一次 WebView2 Cookie 存储（独立线程 + 超时，Windows 上
//!      同步读会死锁）：出现 `BDUSS` 即登录成功，把**全部百度域 Cookie**
//!      （BDUSS / STOKEN / BAIDUID / BAIDUID_BFESS …，含 HttpOnly）拼成
//!      Cookie 头当作回调 `code` 交回网关；
//!   4. 网关 `login.callback` → 插件 `accounts::complete_login`：用完整会话打
//!      `userreport` 复核并取 `uk`，产出账号。
//!
//! ── 为什么必须交回**完整**会话（2026-10-07 浏览器对照实测）────
//! `userreport` 校验完整浏览器会话：只带 BDUSS/STOKEN/gfprotpl 会被判
//! 「未登录」（errno=-6），补上 BAIDUID / BAIDUID_BFESS 后立即可用。
//! 主站方案天然满足（首访即下发 BAIDUID，登录后 BDUSS 一并写入）。
//!
//! ── 迁移时改了什么 ─────────────────────────────────────────
//! 网关那份还有 `new_login_state`（生成一次性 state）与 `build_login_url`。
//! 插件协议里 **flow id 由插件返回、任务 state 由网关持有**
//! （`core::login::tasks`），所以这里不再自己造随机 state —— 直接回一个固定
//! 的 flow 名（`kuku-web`），网关按它建任务。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use agent2api_provider_protocol as proto;

/// 登录流程 id（本家只有一条网页登录流程，固定值即可）。
///
/// ── 为什么可以是常量 ────────────────────────────────────────
/// 网关把 `flow` 当作「后续 poll / callback / cancel 的键」，但它**同时**用
/// 自己的任务表存一次性 state。同名流程并发发起时，后一次会覆盖前一次的
/// 注册项 —— 这与「一个用户同时开两个登录窗口」的现实一致：只有最后那个
/// 窗口的登录态会被收下。真需要并发的话，网关侧的任务表才是要改的地方，
/// 不是这里。
const FLOW_ID: &str = "kuku-web";

/// 发起登录：返回主站地址与窗口约束。
///
/// ── 为什么是主站而不是 passport 登录页（2026-10-07 最终方案）────
/// 早期试过两条路，都被现实否掉：
///   1. `passport.baidu.com/v2/?login&tpl=pp`：没有短信 tab，脚本扑空；
///   2. `passPassport/passApi/html/loginMerge.html`（客户端同款）：能登录，
///      但要在页面里自动填号/发码/填码 —— 页面 DOM 与风控行为随时会变，
///      自动化链路脆弱（脚本注入时机、图形验证码、提交时机都踩过坑）。
/// 主站方案把「怎么登录」还给用户自己：打开 `kuku.baidu.com`，用户用
/// 手机验证码 / 扫码（官方页面自己处理风控），登录成功后壳侧从 WebView2
/// 的 Cookie 存储读**完整会话**交回（含 HttpOnly 的 BDUSS / BAIDUID_BFESS）。
/// 主站对首访即下发 BAIDUID/BAIDUID_BFESS（实测），登录后 BDUSS 一并写入，
/// 因此交回的会话是完整的 —— 这正是 userreport 类接口要求的形态。
pub(super) fn login_start() -> proto::LoginStart {
    proto::LoginStart {
        flow: FLOW_ID.to_string(),
        url: format!("{}/", super::BASE_URL),
        instructions:
            "在弹出的窗口里完成登录（手机验证码 / 扫码均可），登录成功后会自动收下账号"
                .to_string(),
        // 回调型：百度通行证登录成功后不携带授权码，没有可轮询的上游端点
        poll_interval_ms: 0,
        // 内嵌窗口：壳侧要从 WebView2 的 Cookie 存储读登录态 —— 系统浏览器
        // 里读到的 Cookie 壳侧拿不到，这条流程必须走内嵌窗口
        external_browser: false,
        allowed_hosts: vec![
            "kuku.baidu.com".to_string(),
            "passport.baidu.com".to_string(),
            "wappass.baidu.com".to_string(),
            "baidu.com".to_string(),
        ],
    }
}
