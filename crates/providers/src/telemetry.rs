//! 请求级遥测的**空实现**（让从网关搬来的家不必改一行）。
//!
//! ── 这是什么 ────────────────────────────────────────────────
//! 网关的 `RequestTelemetry` 是一个请求级对象：它收集调试报文（`capture`）、
//! 用量（`report_usage`）、内部重试与排队明细（`note_attempt_retry` /
//! `note_queued`）、错误（`note_error`），以及客户端的取消令牌
//! （`cancel_token` / `is_cancelled`）。这些数据的消费方是网关的**请求日志
//! 面板**与编排层（取消传播）。
//!
//! 插件进程里没有那份东西：协议没有「上报遥测」的方法，取消是网关在它自己
//! 那一侧处理的（它把流关掉，插件的 `sink.send` 就会失败）。
//!
//! ── 为什么不删掉调用点 ──────────────────────────────────────
//! 搬来的家（Qoder 的排队退避、piping 的抓包、MonkeyCode 的用量上报）在
//! **热路径**上密集调用这些方法。逐个删掉会让 diff 淹没真正的逻辑，也让
//! 「网关那份与插件这份差在哪」变得难以核对。这里给一个同名的空实现：
//! 调用点一行不改，行为差异集中在本文件说明。
//!
//! ── 差异的后果（如实列出）───────────────────────────────────
//!   1. 调试面板看不到插件家的原始报文（网关侧仍会记录它自己的请求/响应）；
//!   2. 请求日志里没有「排队中」「内部重试」这两类明细（结果与错误仍有）；
//!   3. 排队退避**不可取消**：客户端断开后，插件仍会睡完那一次再发现发不出去
//!      （最多一次退避时长，通常 9～30 秒）；
//!   4. 用量（`report_usage`）不汇总到网关的 token 统计 —— 网关自己的翻译机
//!      认得 OpenAI 帧里的 `usage`，那条路仍然有效。
//!
//! 要消除第 3 条需要给协议加一个「取消通知」方法；要消除 1、2、4 需要给协议
//! 加遥测上报。都是契约变更，不在本次迁移范围内。

use std::sync::Arc;

use serde_json::Value;

/// 调试报文采集器（空实现：`capture()` 恒返回 `None`）。
#[allow(dead_code)]
pub struct Capture;

#[allow(dead_code)]
impl Capture {
    /// 记录一次即将发出的请求（空实现）。
    pub fn reset_request(&self, _url: &str, _provider: &str, _headers: &[(String, String)], _body: &Value) {}

    /// 记录响应状态与头（空实现）。
    pub fn attach_response(&self, _status: u16, _headers: &reqwest::header::HeaderMap) {}

    /// 追加一段响应体字节（空实现）。
    pub fn push(&self, _bytes: &[u8]) {}
}

/// 请求级遥测（空实现，见模块头）。
///
/// ── 为什么整块 allow(dead_code) ────────────────────────────
/// 这个类型的意义是「让搬来的调用点一行不改」。不同家用到的方法不一样
/// （Qoder 用 `note_queued` / `note_attempt_retry`，MonkeyCode 用
/// `report_usage` / `note_error`，抓包只在网关侧有消费方），所以总有几个
/// 方法在当前这批家里没有调用方。逐个加 allow 只会让每个家搬进来时多改
/// 一次；这里是**契约的一部分**，不是遗留代码。
#[allow(dead_code)]
#[derive(Clone, Default)]
pub struct RequestTelemetry;

#[allow(dead_code)]
impl RequestTelemetry {
    /// 新建（插件里恒为一个无状态的空对象）。
    pub fn new() -> Self {
        Self
    }

    /// 调试采集器：**恒为 `None`**（插件不上报调试报文，见模块头第 1 条）。
    pub fn capture(&self) -> Option<Arc<Capture>> {
        None
    }

    /// 上报用量（空实现，见模块头第 4 条）。
    pub fn report_usage(&self, _usage: &Value) {}

    /// 记一次内部重试（空实现，见模块头第 2 条）。
    pub fn note_attempt_retry(&self, _reason: &str, _status: Option<i64>, _delay_ms: u64) {}

    /// 记一次排队等待（空实现，见模块头第 2 条）。
    pub fn note_queued(
        &self,
        _reason: &str,
        _code: Option<i64>,
        _wait_ms: u64,
        _attempt: usize,
        _max_attempts: usize,
    ) {
    }

    /// 记一次错误（空实现：错误本身仍会返回给调用方并进日志）。
    pub fn note_error(&self, _message: &str) {}

    /// 记「收到第一帧内容」的时刻（空实现：网关的时延统计在它自己那一侧算）。
    ///
    /// 从网关搬来的家（Trae 等）在首帧处调它。补一个空实现而不是删调用点，
    /// 理由与模块头一致：让 diff 里只剩真正的逻辑改动。
    pub fn note_first_frame(&self) {}

    /// 客户端取消令牌：**恒为 `None`**（插件拿不到网关的取消信号，见模块头第 3 条）。
    pub fn cancel_token(&self) -> Option<Arc<()>> {
        None
    }

    /// 客户端是否已取消：**恒为 `false`**（理由同 `cancel_token`）。
    pub fn is_cancelled(&self) -> bool {
        false
    }
}
