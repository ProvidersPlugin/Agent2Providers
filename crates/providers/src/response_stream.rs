use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures::{stream::BoxStream, Stream};

use crate::sse_events::{is_done, EventDecoder};

pub(crate) fn sse(response: reqwest::Response) -> BoxStream<'static, Result<Bytes, std::io::Error>> {
    let source = ResponseStream {
        source: Some(Box::pin(response.bytes_stream())),
        events: EventDecoder::default(),
        ended: false,
    };
    crate::stall::idle_guard(
        Box::pin(source),
        Duration::from_millis(crate::timeouts::current().stream_idle_ms()),
    )
}

struct ResponseStream {
    source: Option<BoxStream<'static, Result<Bytes, reqwest::Error>>>,
    events: EventDecoder,
    ended: bool,
}

impl Stream for ResponseStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.ended { return Poll::Ready(None); }
        let polled = match self.source.as_mut() {
            Some(source) => source.as_mut().poll_next(cx),
            None => return Poll::Ready(None),
        };
        match polled {
            Poll::Ready(Some(Ok(bytes))) => {
                let mut done = false;
                let end = self.events.feed(&bytes, |data| { done = is_done(data); done });
                if done {
                    self.ended = true;
                    self.source = None;
                }
                Poll::Ready(Some(Ok(bytes.slice(..end))))
            }
            Poll::Ready(Some(Err(error))) => {
                self.ended = true;
                self.source = None;
                Poll::Ready(Some(Err(std::io::Error::other(crate::http::describe_chain(&error)))))
            }
            Poll::Ready(None) => {
                self.ended = true;
                self.source = None;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
