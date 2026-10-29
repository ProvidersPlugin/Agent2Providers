//! 流式响应的空闲守卫：上游静默超过设定时长就报错收尾。
//!
//! ── 从网关搬来（`core::upstream::stall`）────────────────────
//! 逐字保留行为与文案（[`IDLE_TIMEOUT_PREFIX`] 是网关那边 `ForwardStream`
//! 认的前缀，两边必须一致）。搬过来的唯一改动是把「读设置页」换成
//! `timeouts::current()`（由 SDK 按每次请求隔离保存的网关快照）。
//!
//! ── 为什么需要它 ────────────────────────────────────────────
//! 上游回了 200 之后僵住（不关连接、也不再发数据）时，reqwest 的传输层
//! 后备超时要等很久才触发。用户看到的是「请求挂住了」。这条守卫按「相邻
//! 两块数据之间的最大间隔」判超时，把僵死变成一个可读的错误。

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures::stream::BoxStream;
use futures::Stream;

/// 空闲超时错误文案的前缀（调用方按它区分「保护性判定」与「传输中断」）。
pub const IDLE_TIMEOUT_PREFIX: &str = "流式响应空闲超时";

/// 给一条字节流装上空闲守卫。
pub fn idle_guard(
    inner: BoxStream<'static, Result<Bytes, std::io::Error>>,
    idle: Duration,
) -> BoxStream<'static, Result<Bytes, std::io::Error>> {
    Box::pin(IdleGuard {
        inner,
        idle,
        sleep: Box::pin(tokio::time::sleep(idle)),
        done: false,
    })
}

struct IdleGuard {
    inner: BoxStream<'static, Result<Bytes, std::io::Error>>,
    idle: Duration,
    /// 到点即触发；每收到一块数据就重置
    sleep: Pin<Box<tokio::time::Sleep>>,
    /// 已收尾（超时触发或上游结束）—— 之后的 poll 一律返回 None
    done: bool,
}

impl Stream for IdleGuard {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        match self.inner.as_mut().poll_next(cx) {
            // 收到数据：重置计时器再原样下发（先重置后返回，保证这一块数据
            // 的「处理后空闲」也被计入下一轮）
            Poll::Ready(Some(item)) => {
                let deadline = tokio::time::Instant::now() + self.idle;
                self.sleep.as_mut().reset(deadline);
                Poll::Ready(Some(item))
            }
            Poll::Ready(None) => {
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => match self.sleep.as_mut().poll(cx) {
                // 上游静默超过 idle：报错并收尾（只触发一次）
                Poll::Ready(()) => {
                    self.done = true;
                    Poll::Ready(Some(Err(std::io::Error::other(format!(
                        "{IDLE_TIMEOUT_PREFIX}({}秒)",
                        self.idle.as_secs()
                    )))))
                }
                Poll::Pending => Poll::Pending,
            },
        }
    }
}
