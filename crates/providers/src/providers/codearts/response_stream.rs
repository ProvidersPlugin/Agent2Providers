use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::{stream::BoxStream, Stream};

use crate::errors::GatewayError;
use crate::sse_events::{is_done, EventDecoder};

use super::chat::{head_verdict, HeadVerdict};
use super::tool_names::{Restorer, StreamFixer};

pub(crate) struct ResponseStream {
    source: BoxStream<'static, Result<Bytes, std::io::Error>>,
}

impl ResponseStream {
    pub(crate) fn new(response: reqwest::Response) -> Self {
        Self { source: crate::response_stream::sse(response) }
    }
}

impl Stream for ResponseStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.source.as_mut().poll_next(cx)
    }
}

pub(crate) struct HeadObserver {
    events: EventDecoder,
    verdict: HeadVerdict,
    done: bool,
}

impl Default for HeadObserver {
    fn default() -> Self {
        Self { events: EventDecoder::default(), verdict: HeadVerdict::Empty, done: false }
    }
}

impl HeadObserver {
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        let Self { events, verdict, done } = self;
        events.feed(bytes, |data| {
            if is_done(data) { *done = true; return true; }
            if matches!(verdict, HeadVerdict::Empty) {
                if let Ok(text) = std::str::from_utf8(data) { *verdict = head_verdict([text]); }
            }
            false
        });
    }

    pub(crate) fn finish(&mut self) {
        let verdict = &mut self.verdict;
        self.events.finish(|data| {
            if matches!(verdict, HeadVerdict::Empty) {
                if let Ok(text) = std::str::from_utf8(data) { *verdict = head_verdict([text]); }
            }
            false
        });
    }

    pub(crate) fn ready(&self) -> bool {
        self.done || !matches!(self.verdict, HeadVerdict::Empty)
    }

    pub(crate) fn result(&self, model: &str) -> Result<(), GatewayError> {
        match &self.verdict {
            HeadVerdict::Answered => Ok(()),
            HeadVerdict::Fault(fault) => Err(super::chat::fault_error(fault, model)),
            HeadVerdict::Empty => Err(GatewayError::with_status(502,
                "CodeArts 上游流结束但未返回任何内容（空回答按失败处理，以便换账号重试）")),
        }
    }
}

pub(crate) fn restore(
    prefetched: Vec<u8>,
    source: BoxStream<'static, Result<Bytes, std::io::Error>>,
    restorer: Option<Restorer>,
) -> BoxStream<'static, Result<Bytes, std::io::Error>> {
    Box::pin(RestoredStream { source, fixer: StreamFixer::new(restorer), prefetched: Some(prefetched), pending: VecDeque::new(), ended: false })
}

struct RestoredStream {
    source: BoxStream<'static, Result<Bytes, std::io::Error>>,
    fixer: StreamFixer,
    prefetched: Option<Vec<u8>>,
    pending: VecDeque<Result<Bytes, std::io::Error>>,
    ended: bool,
}

impl RestoredStream {
    fn finish(&mut self, error: Option<std::io::Error>) {
        let tail = self.fixer.finish();
        if !tail.is_empty() { self.pending.push_back(Ok(Bytes::from(tail))); }
        if let Some(error) = error { self.pending.push_back(Err(error)); }
        self.ended = true;
    }
}

impl Stream for RestoredStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(item) = self.pending.pop_front() { return Poll::Ready(Some(item)); }
            if self.ended { return Poll::Ready(None); }
            let polled = if let Some(bytes) = self.prefetched.take() {
                Poll::Ready(Some(Ok(Bytes::from(bytes))))
            } else { self.source.as_mut().poll_next(cx) };
            match polled {
                Poll::Ready(Some(Ok(bytes))) => {
                    if self.fixer.idle() { return Poll::Ready(Some(Ok(bytes))); }
                    let output = self.fixer.push(&bytes);
                    if !output.is_empty() { return Poll::Ready(Some(Ok(Bytes::from(output.into_owned())))); }
                }
                Poll::Ready(Some(Err(error))) => self.finish(Some(error)),
                Poll::Ready(None) => self.finish(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
