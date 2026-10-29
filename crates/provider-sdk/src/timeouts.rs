use std::future::Future;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(default, rename_all = "camelCase")]
pub struct Timeouts {
    pub connect_seconds: i64,
    pub headers_seconds: i64,
    pub stream_idle_seconds: i64,
    pub body_seconds: i64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect_seconds: 30,
            headers_seconds: 300,
            stream_idle_seconds: 300,
            body_seconds: 300,
        }
    }
}

impl Timeouts {
    fn ms(seconds: i64) -> u64 {
        (seconds.max(1) as u64).saturating_mul(1000)
    }

    pub fn connect_ms(&self) -> u64 {
        Self::ms(self.connect_seconds)
    }

    pub fn headers_ms(&self) -> u64 {
        Self::ms(self.headers_seconds)
    }

    pub fn stream_idle_ms(&self) -> u64 {
        Self::ms(self.stream_idle_seconds)
    }

    pub fn body_ms(&self) -> u64 {
        Self::ms(self.body_seconds)
    }

    pub fn read_timeout_backstop_ms(&self) -> u64 {
        self.headers_ms().max(self.stream_idle_ms())
    }
}

tokio::task_local! {
    static CURRENT: Timeouts;
}

/// 每次 RPC 独立持有超时快照；旧网关不传时沿用默认值。
pub fn current() -> Timeouts {
    CURRENT.try_with(|timeouts| *timeouts).unwrap_or_default()
}

pub(crate) async fn scope<F: Future>(timeouts: Timeouts, future: F) -> F::Output {
    CURRENT.scope(timeouts, future).await
}
